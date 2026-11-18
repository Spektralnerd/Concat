// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The beat grid: a song's beats found in the background
//! (`concat_media::beats`), kept on its bin entry, counted into the grid
//! the Beats panel sets, drawn over its clips on the lanes, and snapped to.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_project::beats::{BeatGrid, BeatMark, clip_beats};

use super::*;

/// The grid's knobs as the panel shows them, for a clip with sound.
pub(super) struct BeatReadout {
    pub sound: bool,
    pub progress: f32,
    pub found: bool,
    pub tempo: f32,
    pub count: i32,
    pub confidence: f32,
    pub per_bar: i32,
    pub every: i32,
    pub bars: bool,
    pub offset: f32,
    pub hidden: bool,
}

/// The fewest pixels between two marks of a kind for them to be drawn;
/// closer, a zoomed-out song would be a smear of them.
const MARK_GAP: f64 = 4.0;

impl Studio {
    /// The bin entry of a clip that plays sound - a sound clip, or a
    /// picture with its own sound still on it - which is what holds the
    /// beats it plays.
    fn sounding_media(&self, clip: &Clip) -> Option<&model::MediaItem> {
        let media = self.project().media_by_id(&clip.media_id)?;
        let sounding = match clip.kind {
            model::ClipKind::Audio => true,
            model::ClipKind::Video => media.has_audio && clip.muted != Some(true),
            _ => false,
        };
        sounding.then_some(media)
    }

    /// The bin entry behind the one clip selected, when it plays sound.
    fn selected_sound(&self) -> Option<model::MediaItem> {
        let id = self.sole_selection()?;
        let clip = self.clip(&id)?;
        self.sounding_media(clip).cloned()
    }

    /// Finds the beats of the selected clip's sound, in the background; a
    /// grid already counted keeps how it is counted. One run a file at a
    /// time.
    pub fn beats_detect(&mut self) {
        self.find_beats(None);
    }

    /// Finds the beats of the selected clip's sound again as one steady
    /// row at about `bpm` beats per minute, typed in: for a song the finder
    /// counts wrong, or whose beat it lets wander through an intro.
    pub fn beats_detect_at(&mut self, bpm: f32) {
        if bpm.is_finite() && bpm > 0.0 {
            self.find_beats(Some(f64::from(bpm)));
        }
    }

    /// See [`Studio::beats_detect`]: at `tempo` when it is given.
    fn find_beats(&mut self, tempo: Option<f64>) {
        let Some(id) = self.sole_selection() else {
            return;
        };
        let Some(clip) = self.clip(&id).cloned() else {
            return;
        };
        let Some(media) = self.sounding_media(&clip).cloned() else {
            return;
        };
        if self.beat_jobs.contains_key(&media.id) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.beat_jobs.insert(media.id.clone(), 0.0);
        self.beat_cancels
            .insert(media.id.clone(), Arc::clone(&cancel));
        match tempo {
            Some(bpm) => log::info!("beats: laying a steady {bpm:.2} BPM over {}", media.name),
            None => log::info!("beats: looking for the beats of {}", media.name),
        }
        let epoch = crate::host::project_epoch();
        let media_id = media.id.clone();
        let stream = clip.audio_stream.map(|stream| stream as usize);
        spawn_in_project(
            move || {
                let mut last = -1.0f32;
                let reporting = media_id.clone();
                let result = concat_media::beats::analyse_file(
                    std::path::Path::new(&media.path),
                    stream,
                    media.duration,
                    tempo,
                    &mut |fraction| {
                        // Every percent, not every chunk.
                        if fraction - last >= 0.01 {
                            last = fraction;
                            let id = reporting.clone();
                            on_ui_in_project(epoch, move |studio, _, _| {
                                if let Some(held) = studio.beat_jobs.get_mut(&id) {
                                    *held = fraction;
                                }
                            });
                        }
                    },
                    &cancel,
                );
                (media_id, result)
            },
            |studio, _, _, (media_id, result)| studio.beats_landed(media_id, result),
        );
    }

    /// A detection's end: the beats kept on the file, counted the way its
    /// grid was before, or in bars of four with a line every bar; nothing
    /// for one stopped; a word on screen for one that found nothing or
    /// failed.
    fn beats_landed(
        &mut self,
        media_id: String,
        result: concat_media::Result<Option<concat_media::beats::Analysis>>,
    ) {
        self.beat_jobs.remove(&media_id);
        self.beat_cancels.remove(&media_id);
        let found = match result {
            Ok(Some(found)) => found,
            Ok(None) => {
                log::info!("beats: stopped");
                return;
            }
            Err(error) => {
                log::warn!("beats: the sound could not be read: {error}");
                self.notify(&tf("beats.failed", &[&error]), true);
                return;
            }
        };
        if found.beats.is_empty() {
            log::info!("beats: none found");
            self.notify(&t("beats.noneFound"), true);
            return;
        }
        let before = self
            .project()
            .media_by_id(&media_id)
            .and_then(|media| media.beats.clone())
            .unwrap_or_default();
        let count = found.beats.len();
        let tempo = found.tempo;
        log::info!(
            "beats: {count} beats at {tempo:.1} BPM, {:.0} % on a hit",
            found.confidence * 100.0
        );
        let mut grid = BeatGrid::found(
            found.tempo,
            found.beats,
            found.accents,
            found.confidence,
            before.per_bar,
            before.every,
            before.bars,
        );
        grid.offset = before.offset;
        grid.hidden = false;
        self.apply(Command::SetMediaBeats {
            media_id,
            beats: Some(grid),
        });
        self.notify(&tf("beats.found", &[&format!("{tempo:.1}"), &count]), false);
    }

    /// Stops the detection running for the selected clip's sound.
    pub fn beats_cancel(&mut self) {
        let Some(media) = self.selected_sound() else {
            return;
        };
        if let Some(cancel) = self.beat_cancels.get(&media.id) {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Stops every detection: the project is closing.
    pub(super) fn cancel_beats(&mut self) {
        for cancel in self.beat_cancels.values() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.beat_cancels.clear();
        self.beat_jobs.clear();
    }

    /// Changes the selected clip's grid through `change`, as one edit, or
    /// as one step of a knob's turn when `gesture` names it.
    fn change_grid(&mut self, gesture: Option<&str>, change: impl FnOnce(&BeatGrid) -> BeatGrid) {
        let Some(media) = self.selected_sound() else {
            return;
        };
        let Some(grid) = media.beats.as_ref() else {
            return;
        };
        let changed = change(grid);
        if &changed == grid {
            return;
        }
        let command = Command::SetMediaBeats {
            media_id: media.id.clone(),
            beats: Some(changed),
        };
        let gesture = gesture.map(|name| format!("{name}:{}", media.id));
        self.apply_in(gesture.as_deref(), command);
    }

    /// Counts the grid: `per_bar` beats a bar, a line every `every` bars,
    /// or beats, moved by `offset` milliseconds, kept off the lanes when
    /// `hidden`. A new number of beats a bar finds its own first beat.
    pub fn beats_set(&mut self, per_bar: i32, every: i32, bars: bool, offset: f32, hidden: bool) {
        self.change_grid(Some("beat grid"), |grid| {
            let mut next = grid.clone();
            let per_bar = per_bar.clamp(1, BeatGrid::MOST as i32) as u32;
            if per_bar != grid.per_bar {
                next.count_in(per_bar);
            }
            next.every = every.clamp(1, BeatGrid::MOST as i32) as u32;
            next.bars = bars;
            next.offset = f64::from(offset) / 1000.0;
            next.hidden = hidden;
            next.tidy();
            next
        });
    }

    /// Moves the counting on by `beats` beats, back when negative.
    pub fn beats_shift(&mut self, beats: i32) {
        self.change_grid(None, |grid| {
            let mut next = grid.clone();
            next.shift(i64::from(beats));
            next
        });
    }

    /// Doubles the tempo for a positive `factor`, halves it for a negative.
    pub fn beats_tempo(&mut self, factor: i32) {
        self.change_grid(None, |grid| {
            if factor > 0 {
                grid.doubled()
            } else if grid.beats.len() > 2 {
                grid.halved()
            } else {
                grid.clone()
            }
        });
    }

    /// Drops the selected clip's beats.
    pub fn beats_clear(&mut self) {
        let Some(media) = self.selected_sound() else {
            return;
        };
        if media.beats.is_some() {
            self.apply(Command::SetMediaBeats {
                media_id: media.id,
                beats: None,
            });
        }
    }

    /// What the Beats panel shows for `clip`.
    pub(super) fn beat_readout(&self, clip: &Clip) -> BeatReadout {
        let media = self.sounding_media(clip);
        let grid = media.and_then(|media| media.beats.as_ref());
        BeatReadout {
            sound: media.is_some(),
            progress: media
                .and_then(|media| self.beat_jobs.get(&media.id))
                .copied()
                .unwrap_or(-1.0),
            found: grid.is_some_and(|grid| !grid.beats.is_empty()),
            tempo: grid.map_or(0.0, |grid| grid.tempo as f32),
            count: grid.map_or(0, |grid| grid.beats.len() as i32),
            confidence: grid.map_or(0.0, |grid| grid.confidence),
            per_bar: grid.map_or(4, |grid| grid.per_bar as i32),
            every: grid.map_or(1, |grid| grid.every as i32),
            bars: grid.is_none_or(|grid| grid.bars),
            offset: grid.map_or(0.0, |grid| (grid.offset * 1000.0) as f32),
            hidden: grid.is_some_and(|grid| grid.hidden),
        }
    }

    /// The lines of every beat grid on the lanes, in timeline seconds:
    /// what cuts snap to, beside the clips' edges. A grid kept off the
    /// lanes snaps nothing, and the clips `skip` names - the ones being
    /// moved - lend none.
    pub(super) fn beat_targets(&self, skip: impl Fn(&str) -> bool) -> Vec<f32> {
        let mut targets = Vec::new();
        for clip in &self.timeline().clips {
            if skip(&clip.id) {
                continue;
            }
            let Some(media) = self.sounding_media(clip) else {
                continue;
            };
            if media.beats.as_ref().is_none_or(|grid| grid.hidden) {
                continue;
            }
            targets.extend(
                clip_beats(clip, media)
                    .into_iter()
                    .filter(|(_, mark)| *mark == BeatMark::Line)
                    .map(|(at, _)| at as f32),
            );
        }
        targets
    }

    /// The grid drawn over a clip on the lanes: its lines, and its other
    /// beats as marks at the foot, as path commands in a 1x1 box over the
    /// whole clip, each a pixel or two wide at the lanes' zoom. Empty for a
    /// clip without a grid, or with one kept off the lanes; a kind of mark
    /// that would stand closer than a few pixels is left out.
    pub(super) fn beat_paths(&self, clip: &Clip) -> (SharedString, SharedString) {
        let Some(media) = self.sounding_media(clip) else {
            return Default::default();
        };
        let Some(grid) = media.beats.as_ref().filter(|grid| !grid.hidden) else {
            return Default::default();
        };
        let duration = clip.duration;
        let seconds_per_pixel = f64::from(self.lanes.seconds_per_pixel);
        if duration <= 0.0 || seconds_per_pixel <= 0.0 || grid.beats.len() < 2 {
            return Default::default();
        }
        let key = format!(
            "{}|{}|{}|{:.4}|{:.4}|{:.6}",
            media.id, clip.id, self.revision, clip.source_start, duration, seconds_per_pixel
        );
        if let Some(cached) = self.beat_marks.borrow().get(&key) {
            return cached.clone();
        }
        // Pixels from beat to beat, as the clip plays them.
        let source_gap =
            (grid.beats[grid.beats.len() - 1] - grid.beats[0]) / (grid.beats.len() - 1) as f64;
        let rate = match clip.speed_curve.as_deref() {
            Some(points) => concat_project::speed::average_speed(points),
            None => clip.speed,
        };
        let beat_px = source_gap / rate.max(1e-6) / seconds_per_pixel;
        let draw_beats = beat_px >= MARK_GAP;
        let draw_bars = beat_px * f64::from(grid.per_bar) >= MARK_GAP;
        let draw_lines = beat_px * grid.cycle() as f64 >= MARK_GAP;
        let pixel = seconds_per_pixel / duration;
        let mut lines = String::new();
        let mut ticks = String::new();
        for (at, mark) in clip_beats(clip, media) {
            let x = (at - clip.start) / duration;
            let (target, top, half) = match mark {
                BeatMark::Line if draw_lines => (&mut lines, 0.0, pixel),
                BeatMark::Bar if draw_bars => (&mut ticks, 0.45, pixel / 2.0),
                BeatMark::Beat if draw_beats => (&mut ticks, 0.75, pixel / 2.0),
                _ => continue,
            };
            let (left, right) = (x - half, x + half);
            let _ = write!(
                target,
                "M{left:.6} {top}L{right:.6} {top}L{right:.6} 1L{left:.6} 1Z"
            );
        }
        let built = (SharedString::from(lines), SharedString::from(ticks));
        let mut marks = self.beat_marks.borrow_mut();
        if marks.len() >= 256 {
            marks.clear();
        }
        marks.insert(key, built.clone());
        built
    }
}
