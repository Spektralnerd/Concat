// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: the Scenes sheet - a film's scenes, picked and laid on the
//! timeline as clips of the film.
//!
//! The list comes from beside the film (`concat_host::scenes`): one made
//! here, or by PySceneDetect or the Szenenschnitt apps. Where there is
//! none, the film is read through once (`concat_media::scenes`), on a
//! thread of its own since it takes as long as the film takes to decode,
//! and the list and the pictures are written beside it. One read at a
//! time, for the whole app: a project closed under it does not stop it,
//! and the sheet of any project that shows the film shows how far it is.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use concat_host::scenes::{self as files, Manifest, PictureWriter, Pictures, SceneList};
use concat_media::scenes::{self as detector, Event, Settings};
use concat_project::commands::ClipSpan;

use super::*;

/// Scenes shown a page at a time: ten rows of three.
const PAGE: usize = 30;

/// Pictures this many pixels across: a third of a phone held upright.
const PICTURE_WIDTH: u32 = 320;

/// The sheet's film, and what is known of its scenes.
pub(crate) struct ScenesView {
    media_id: String,
    path: PathBuf,
    name: String,
    loading: bool,
    list: Option<Arc<SceneList>>,
    /// Where the list was read from.
    source: Option<PathBuf>,
    pictures: Option<Arc<Pictures>>,
    /// The film's frame rate as the timeline counts it: the seconds a
    /// scene's frames are at.
    fps: f64,
    page: usize,
    /// Scenes picked, by index, in the order they were picked.
    picked: Vec<usize>,
    /// The shown page's pictures, by scene.
    images: HashMap<usize, slint::Image>,
    /// The page those are of, or are on their way for.
    images_page: Option<usize>,
    /// The scene last played on the monitor.
    playing: Option<usize>,
}

/// A read through a film in the background.
struct Scan {
    path: PathBuf,
    /// Pictures for a list there is, rather than a list.
    painting: bool,
    frames: u64,
    total: u64,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

static SCAN: Mutex<Option<Scan>> = Mutex::new(None);

fn scan() -> std::sync::MutexGuard<'static, Option<Scan>> {
    SCAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The detection's knobs, remembered for the next film while the app runs.
static KNOBS: Mutex<(f32, f32)> = Mutex::new((27.0, 0.6));

fn knobs() -> (f32, f32) {
    *KNOBS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The page each film's sheet was last on, while the app runs: the sheet
/// opens again where it was left.
static PAGES: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

fn remembered_page(path: &Path) -> usize {
    PAGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|pages| pages.get(path).copied())
        .unwrap_or(0)
}

fn remember_page(path: &Path, page: usize) {
    PAGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), page);
}

/// What the sheet shows, for the publish.
pub(super) struct ScenesReadout {
    pub title: String,
    pub note: String,
    pub loading: bool,
    pub listed: bool,
    pub pictured: bool,
    pub cards: Vec<SceneCardData>,
    pub page: i32,
    pub pages: i32,
    pub picked: i32,
    pub busy: bool,
    pub busy_here: bool,
    /// The scene playing on the monitor, or -1.
    pub playing: i32,
    pub progress: f32,
    pub progress_text: String,
    pub threshold: f32,
    pub min_seconds: f32,
    /// The film's height over its width.
    pub aspect: f32,
}

/// A length in seconds with one decimal, the decimal mark the
/// interface's language writes.
fn length(seconds: f64) -> String {
    let number = format!("{seconds:.1}");
    let number = if i18n::current() == "de" {
        number.replace('.', ",")
    } else {
        number
    };
    tf("scenes.seconds", &[&number])
}

/// A picture as JPEG, for the ZIP.
fn jpeg(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(24 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 82)
        .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// The film's frame rate as the timeline counts it, `fraction` being the
/// probe's exact one ("24000/1001"): the list's own where they differ, as
/// a list made of another cut of the film would.
fn timeline_fps(media: &model::MediaItem, list: &SceneList) -> f64 {
    let probed = media
        .frame_rate_fraction
        .as_deref()
        .and_then(|fraction| {
            let (num, den) = fraction.split_once('/')?;
            let (num, den) = (
                num.trim().parse::<f64>().ok()?,
                den.trim().parse::<f64>().ok()?,
            );
            (num > 0.0 && den > 0.0).then_some(num / den)
        })
        .or(media.frame_rate)
        .filter(|fps| *fps > 0.0);
    match probed {
        // Times written to a thousandth of a second carry the rate to a
        // few parts in a million over a long film; the probe's is exact.
        Some(probed) if list.fps <= 0.0 || (probed - list.fps).abs() / probed < 0.002 => probed,
        _ if list.fps > 0.0 => list.fps,
        Some(probed) => probed,
        None => 25.0,
    }
}

impl Studio {
    /// The sheet opens on the bin's one picked film.
    pub fn scenes_open(&mut self) {
        let Some(media) = self.media.sole_film(self.project()).cloned() else {
            return;
        };
        let path = PathBuf::from(&media.path);
        self.scenes = Some(ScenesView {
            media_id: media.id.clone(),
            path: path.clone(),
            name: media.name.clone(),
            loading: true,
            list: None,
            source: None,
            pictures: None,
            fps: 25.0,
            page: remembered_page(&path),
            picked: Vec::new(),
            images: HashMap::new(),
            images_page: None,
            playing: None,
        });
        self.scenes_load(path);
    }

    /// The sheet closed: the monitor goes back to the timeline from a
    /// scene it was showing.
    pub fn scenes_close(&mut self) {
        if let Some(view) = self.scenes.take()
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.media_id == view.media_id && source.card.is_empty())
        {
            self.close_source();
        }
    }

    /// Plays scene `scene` on the monitor, from its first frame to its
    /// last, with its sound; pauses it when it is the one playing.
    pub fn scenes_play(&mut self, scene: i32) {
        let Some((media_id, list, fps, playing)) = self.scenes.as_ref().and_then(|view| {
            Some((
                view.media_id.clone(),
                view.list.clone()?,
                view.fps,
                view.playing,
            ))
        }) else {
            return;
        };
        let Ok(scene) = usize::try_from(scene) else {
            return;
        };
        if scene >= list.len() {
            return;
        }
        let shown = self
            .source
            .as_ref()
            .filter(|source| source.media_id == media_id && source.card.is_empty());
        if playing == Some(scene) && shown.is_some_and(|source| source.playing) {
            self.source_toggle();
            return;
        }
        if shown.is_none() {
            let row = self.media.row_of(&media_id).unwrap_or(-1);
            self.preview_media(&media_id, row);
        }
        let (start, end) = list.scene(scene);
        let Some(source) = self.source.as_mut() else {
            return;
        };
        // Half a frame inside it at either end: the scene's own first and
        // last frames, never the last of the scene before or the first of
        // the one after.
        source.stop_at = Some((end as f64 - 0.5) / fps);
        self.source_seek((start as f64 + 0.5) / fps);
        if self.source.as_ref().is_some_and(|source| !source.playing) {
            self.source_toggle();
        }
        if let Some(view) = self.scenes.as_mut() {
            view.playing = Some(scene);
        }
    }

    /// Looks for the film's list and pictures, in the background.
    fn scenes_load(&mut self, path: PathBuf) {
        spawn_in_project(
            move || {
                let found = files::find_list(&path);
                let pictures = found
                    .as_ref()
                    .and_then(|(_, list)| Pictures::open(&path, list));
                (path, found, pictures)
            },
            |studio, _, _, (path, found, pictures)| studio.scenes_loaded(&path, found, pictures),
        );
    }

    fn scenes_loaded(
        &mut self,
        path: &Path,
        found: Option<(PathBuf, SceneList)>,
        pictures: Option<Pictures>,
    ) {
        let media = self
            .scenes
            .as_ref()
            .and_then(|view| self.project().media_by_id(&view.media_id))
            .cloned();
        let Some(view) = self.scenes.as_mut().filter(|view| view.path == path) else {
            return;
        };
        view.loading = false;
        match found {
            Some((source, list)) => {
                if let Some(media) = media.as_ref() {
                    view.fps = timeline_fps(media, &list);
                }
                let pages = list.len().div_ceil(PAGE).max(1);
                view.page = view.page.min(pages - 1);
                view.picked.retain(|&scene| scene < list.len());
                log::info!(
                    "scenes: {} scenes of {} from {}",
                    list.len(),
                    view.name,
                    source.display()
                );
                view.list = Some(Arc::new(list));
                view.source = Some(source);
            }
            None => {
                view.list = None;
                view.source = None;
                view.picked.clear();
            }
        }
        view.pictures = pictures.map(Arc::new);
        view.images.clear();
        view.images_page = None;
        self.scenes_fetch();
    }

    /// Reads and decodes the shown page's pictures, in the background.
    fn scenes_fetch(&mut self) {
        let Some(view) = self.scenes.as_mut() else {
            return;
        };
        let (Some(list), Some(pictures)) = (view.list.clone(), view.pictures.clone()) else {
            return;
        };
        let page = view.page;
        if view.images_page == Some(page) {
            return;
        }
        view.images_page = Some(page);
        view.images.clear();
        let path = view.path.clone();
        let scenes = page * PAGE..((page + 1) * PAGE).min(list.len());
        spawn_in_project(
            move || {
                scenes
                    .filter_map(|scene| {
                        let bytes = pictures.read(&list, scene, 2)?;
                        let picture =
                            image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
                                .ok()?
                                .into_rgb8();
                        Some((
                            scene,
                            slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(
                                picture.as_raw(),
                                picture.width(),
                                picture.height(),
                            ),
                        ))
                    })
                    .collect::<Vec<_>>()
            },
            move |studio, _, _, decoded| {
                let Some(view) = studio
                    .scenes
                    .as_mut()
                    .filter(|view| view.path == path && view.page == page)
                else {
                    return;
                };
                view.images = decoded
                    .into_iter()
                    .map(|(scene, buffer)| (scene, slint::Image::from_rgb8(buffer)))
                    .collect();
            },
        );
    }

    /// Turns to page `page`, from 0.
    pub fn scenes_page_to(&mut self, page: i32) {
        let Some(view) = self.scenes.as_mut() else {
            return;
        };
        let pages = view
            .list
            .as_ref()
            .map_or(1, |list| list.len().div_ceil(PAGE).max(1));
        let page = (page.max(0) as usize).min(pages - 1);
        if page != view.page {
            view.page = page;
            remember_page(&view.path, page);
            self.scenes_fetch();
        }
    }

    /// Picks scene `scene`, or lets it go.
    pub fn scenes_pick(&mut self, scene: i32) {
        let Some(view) = self.scenes.as_mut() else {
            return;
        };
        let Ok(scene) = usize::try_from(scene) else {
            return;
        };
        if view.list.as_ref().is_none_or(|list| scene >= list.len()) {
            return;
        }
        match view.picked.iter().position(|&held| held == scene) {
            Some(at) => {
                view.picked.remove(at);
            }
            None => view.picked.push(scene),
        }
    }

    /// Lets every pick go.
    pub fn scenes_clear_picks(&mut self) {
        if let Some(view) = self.scenes.as_mut() {
            view.picked.clear();
        }
    }

    /// Lays the picked scenes down in the order they were picked, as the
    /// bin's Add lays files: onto the main lane at the playhead, as one
    /// step. True when anything landed.
    pub fn scenes_add(&mut self, at_end: bool) -> bool {
        let Some((media_id, list, fps, picked)) = self.scenes.as_ref().and_then(|view| {
            Some((
                view.media_id.clone(),
                view.list.clone()?,
                view.fps,
                view.picked.clone(),
            ))
        }) else {
            return false;
        };
        if picked.is_empty() || self.project().media_by_id(&media_id).is_none() {
            return false;
        }
        if let Some(view) = self.scenes.as_mut() {
            view.picked.clear();
        }
        let spans: Vec<ClipSpan> = picked
            .into_iter()
            .map(|scene| {
                let (start, end) = list.scene(scene);
                ClipSpan {
                    media_id: media_id.clone(),
                    source_start: start as f64 / fps,
                    duration: (end - start) as f64 / fps,
                }
            })
            .collect();
        let count = spans.len();
        let (track_id, start, ripple) = crate::panes::media_bin::add_place(self, false, at_end);
        let before = crate::panes::media_bin::clip_ids(self);
        self.close_source();
        self.apply(Command::AddClipSpans {
            spans,
            track_id,
            start,
            ripple,
        });
        crate::panes::media_bin::follow_landed(self, &before);
        log::info!("scenes: {count} scenes laid down");
        true
    }

    /// Reads the film through for its scenes, with `threshold` and the
    /// shortest scene `min_seconds` long, writing the list and its pictures
    /// beside it.
    pub fn scenes_detect(&mut self, threshold: f32, min_seconds: f32) {
        let threshold = if threshold.is_finite() && threshold > 0.0 {
            threshold
        } else {
            27.0
        };
        let min_seconds = if min_seconds.is_finite() {
            min_seconds.max(0.0)
        } else {
            0.6
        };
        *KNOBS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = (threshold, min_seconds);
        let settings = Settings {
            threshold: f64::from(threshold),
            min_seconds: f64::from(min_seconds),
            merge: true,
        };
        self.scenes_start(false, move |path, cancel, report| {
            detect_into_files(path, &settings, cancel, report)
        });
    }

    /// Makes the pictures of the list there is, which came without.
    pub fn scenes_paint(&mut self) {
        let Some(list) = self.scenes.as_ref().and_then(|view| view.list.clone()) else {
            return;
        };
        self.scenes_start(true, move |path, cancel, report| {
            paint_into_files(path, &list, cancel, report)
        });
    }

    /// Starts a read through the sheet's film on a thread of its own:
    /// `work` with the film, the stop and a progress report.
    fn scenes_start(
        &mut self,
        painting: bool,
        work: impl FnOnce(&Path, &AtomicBool, &mut dyn FnMut(u64, u64)) -> Result<bool, String>
        + Send
        + 'static,
    ) {
        let Some(path) = self.scenes.as_ref().map(|view| view.path.clone()) else {
            return;
        };
        let cancel = {
            let mut running = scan();
            if running.is_some() {
                self.notify(&t("scenes.oneAtATime"), true);
                return;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            *running = Some(Scan {
                path: path.clone(),
                painting,
                frames: 0,
                total: 0,
                started: Instant::now(),
                cancel: Arc::clone(&cancel),
            });
            cancel
        };
        log::info!(
            "scenes: reading {} for its {}",
            path.display(),
            if painting { "pictures" } else { "scenes" }
        );
        let started = std::thread::Builder::new()
            .name("concat-scenes".to_owned())
            .spawn(move || {
                let _awake = concat_host::awake::Awake::hold();
                let mut said = Instant::now();
                let mut report = |frames: u64, total: u64| {
                    if let Some(scan) = scan().as_mut() {
                        scan.frames = frames;
                        scan.total = total;
                    }
                    // Twice a second is enough for a bar.
                    if said.elapsed().as_millis() >= 500 {
                        said = Instant::now();
                        crate::host::on_ui(|_, _, _| {});
                    }
                };
                let result = work(&path, &cancel, &mut report);
                *scan() = None;
                crate::host::on_ui(move |studio, _, _| studio.scenes_read(&path, result));
            });
        if let Err(error) = started {
            *scan() = None;
            self.notify(&tf("scenes.failed", &[&error]), true);
        }
    }

    /// Stops the read under way.
    pub fn scenes_cancel(&mut self) {
        if let Some(scan) = scan().as_ref() {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// A read through `path` ended: done (true), stopped (false) or failed.
    fn scenes_read(&mut self, path: &Path, result: Result<bool, String>) {
        match result {
            Ok(true) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.notify(&tf("scenes.done", &[&name]), false);
                if let Some(view) = self.scenes.as_mut().filter(|view| view.path == path) {
                    view.loading = true;
                    view.images_page = None;
                    let path = view.path.clone();
                    self.scenes_load(path);
                }
            }
            Ok(false) => log::info!("scenes: stopped"),
            Err(error) => {
                log::warn!("scenes: {error}");
                self.notify(&tf("scenes.failed", &[&error]), true);
            }
        }
    }

    /// Whether the bin's pick is a film the Scenes sheet can open on.
    pub(super) fn scenes_offered(&self) -> bool {
        self.media.sole_film(self.project()).is_some()
    }

    /// What the Scenes sheet shows.
    pub(super) fn scenes_readout(&self) -> ScenesReadout {
        let (threshold, min_seconds) = knobs();
        let running = scan();
        let mut readout = ScenesReadout {
            title: String::new(),
            note: String::new(),
            loading: false,
            listed: false,
            pictured: false,
            cards: Vec::new(),
            page: 0,
            pages: 1,
            picked: 0,
            busy: running.is_some(),
            busy_here: false,
            playing: -1,
            progress: 0.0,
            progress_text: String::new(),
            threshold,
            min_seconds,
            aspect: 0.5625,
        };
        let Some(view) = self.scenes.as_ref() else {
            return readout;
        };
        readout.title = tf("scenes.title", &[&view.name]);
        if let Some(media) = self.project().media_by_id(&view.media_id)
            && let (Some(width), Some(height)) = (media.width, media.height)
            && width > 0
        {
            readout.aspect = height as f32 / width as f32;
        }
        readout.loading = view.loading;
        if let Some(scan) = running.as_ref() {
            readout.busy_here = scan.path == view.path;
            let fraction = if scan.total > 0 {
                (scan.frames as f64 / scan.total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            readout.progress = fraction as f32;
            let elapsed = scan.started.elapsed().as_secs_f64();
            let rate = if elapsed > 0.5 {
                scan.frames as f64 / elapsed
            } else {
                0.0
            };
            let left = if rate > 0.0 {
                crate::format::span(scan.total.saturating_sub(scan.frames) as f64 / rate)
            } else {
                "…".to_owned()
            };
            let film = scan
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            readout.progress_text = if readout.busy_here {
                tf(
                    if scan.painting {
                        "scenes.painting"
                    } else {
                        "scenes.reading"
                    },
                    &[
                        &format!("{:.0}", fraction * 100.0),
                        &format!("{rate:.0}"),
                        &left,
                    ],
                )
            } else {
                tf("scenes.busyElsewhere", &[&film])
            };
        }
        let Some(list) = view.list.as_ref() else {
            readout.note = if view.loading {
                t("scenes.looking")
            } else {
                t("scenes.none")
            };
            return readout;
        };
        readout.listed = true;
        readout.pictured = view
            .pictures
            .as_ref()
            .is_some_and(|pictures| pictures.complete());
        let source = view
            .source
            .as_ref()
            .and_then(|source| source.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        readout.note = if view.pictures.is_some() {
            tf("scenes.from", &[&list.len(), &source])
        } else {
            tf("scenes.fromNoPictures", &[&list.len(), &source])
        };
        readout.pages = list.len().div_ceil(PAGE).max(1) as i32;
        if let Some(scene) = view.playing
            && self.source.as_ref().is_some_and(|source| {
                source.playing && source.media_id == view.media_id && source.card.is_empty()
            })
        {
            readout.playing = scene as i32;
        }
        readout.page = view.page as i32;
        readout.picked = view.picked.len() as i32;
        let first = view.page * PAGE;
        readout.cards = (first..((view.page + 1) * PAGE).min(list.len()))
            .map(|scene| {
                let (start, end) = list.scene(scene);
                let picture = view.images.get(&scene).cloned();
                SceneCardData {
                    index: scene as i32,
                    number: scene as i32 + 1,
                    pictured: picture.is_some(),
                    picture: picture.unwrap_or_default(),
                    time: crate::format::running_time(start as f64 / view.fps).into(),
                    length: length((end - start) as f64 / view.fps).into(),
                    pick: view
                        .picked
                        .iter()
                        .position(|&held| held == scene)
                        .map_or(0, |at| at as i32 + 1),
                }
            })
            .collect();
        readout
    }
}

/// Where a film's list and pictures go: beside it, or the app's folder
/// where its own takes no files. The list's place, and a writer of the
/// pictures' ZIP.
fn places(film: &Path) -> Result<(PathBuf, PictureWriter), String> {
    let csv = files::csv_beside(film);
    let zip = files::zip_beside(film);
    match PictureWriter::create(&zip, film) {
        Ok(writer) => Ok((csv, writer)),
        Err(error) => {
            let dir = files::fallback_dir(film).ok_or_else(|| error.clone())?;
            log::info!("scenes: {error}; writing to {} instead", dir.display());
            let csv = dir.join(csv.file_name().unwrap_or_default());
            let zip = dir.join(zip.file_name().unwrap_or_default());
            Ok((csv, PictureWriter::create(&zip, film)?))
        }
    }
}

/// The detection itself: the film read through, its pictures written as
/// they come, the list written at the end. False when stopped.
fn detect_into_files(
    film: &Path,
    settings: &Settings,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(u64, u64),
) -> Result<bool, String> {
    let (csv, mut writer) = places(film)?;
    let mut failed = None;
    let found = detector::detect(
        film,
        settings,
        Some(PICTURE_WIDTH),
        &mut |event| match event {
            Event::Progress { frames, total } => report(frames, total),
            Event::Picture {
                scene,
                index,
                width,
                height,
                rgb,
                ..
            } => {
                if failed.is_some() {
                    return;
                }
                if let Err(error) =
                    jpeg(rgb, width, height).and_then(|bytes| writer.add(scene, index, &bytes))
                {
                    // A full card, most likely: stop rather than read on
                    // for a list with nowhere to go.
                    failed = Some(error);
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        },
        cancel,
    );
    if let Some(error) = failed {
        writer.abandon();
        return Err(error);
    }
    let found = match found {
        Ok(Some(found)) => found,
        Ok(None) => {
            writer.abandon();
            return Ok(false);
        }
        Err(error) => {
            writer.abandon();
            return Err(error.to_string());
        }
    };
    let list = SceneList::from_cuts(&found.cuts, found.frames, found.fps);
    log::info!(
        "scenes: {} scenes in {} frames of {}",
        list.len(),
        found.frames,
        film.display()
    );
    files::write_csv(&csv, &list)?;
    writer.finish(&Manifest {
        fps: found.fps,
        frames: found.frames,
        cuts: list.cuts().to_vec(),
        threshold: Some(settings.threshold),
        min_seconds: Some(settings.min_seconds),
    })?;
    Ok(true)
}

/// The pictures of a list made elsewhere, read through the film once.
fn paint_into_files(
    film: &Path,
    list: &SceneList,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(u64, u64),
) -> Result<bool, String> {
    let (_, mut writer) = places(film)?;
    let scenes: Vec<(u64, u64)> = (0..list.len()).map(|scene| list.scene(scene)).collect();
    let mut failed = None;
    let done = detector::pictures(
        film,
        &scenes,
        PICTURE_WIDTH,
        &mut |event| match event {
            Event::Progress { frames, total } => report(frames, total.min(list.end.max(frames))),
            Event::Picture {
                scene,
                index,
                width,
                height,
                rgb,
                ..
            } => {
                if failed.is_some() {
                    return;
                }
                if let Err(error) =
                    jpeg(rgb, width, height).and_then(|bytes| writer.add(scene, index, &bytes))
                {
                    failed = Some(error);
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        },
        cancel,
    );
    if let Some(error) = failed {
        writer.abandon();
        return Err(error);
    }
    match done {
        Ok(true) => {
            writer.finish(&Manifest {
                fps: list.fps,
                frames: list.end,
                cuts: list.cuts().to_vec(),
                threshold: None,
                min_seconds: None,
            })?;
            Ok(true)
        }
        Ok(false) => {
            writer.abandon();
            Ok(false)
        }
        Err(error) => {
            writer.abandon();
            Err(error.to_string())
        }
    }
}
