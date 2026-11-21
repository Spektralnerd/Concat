// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: an edit's cuts as lines to cut to - another film cut to the
//! rhythm of an edit already made. The edit's picture is read through in
//! the background (`concat_media::scenes`, the detector the Scenes sheet
//! uses, without its pictures), the moments its shots change kept on its
//! bin entry (`CutMarks`), drawn across its clips on the lanes and snapped
//! to as the beat grid's lines are.

use std::sync::atomic::{AtomicBool, Ordering};

use concat_media::scenes::{Event, Settings};
use concat_project::beats::{CutMarks, clip_cuts};

use super::*;

/// What the Cuts panel shows for the selected clip.
pub(super) struct CutReadout {
    /// The clip is a picture with a file behind it, whose cuts can be found.
    pub footage: bool,
    /// The reading under way, -1 when none, else how far along.
    pub progress: f32,
    pub found: bool,
    pub count: i32,
    pub hidden: bool,
}

/// The fewest pixels between two cut lines for them to be drawn.
const LINE_GAP: f64 = 3.0;

impl Studio {
    /// The bin entry behind a clip with a picture from a file: what has
    /// cuts to find.
    fn footage_of(&self, clip: &Clip) -> Option<&model::MediaItem> {
        (clip.kind == model::ClipKind::Video)
            .then(|| self.project().media_by_id(&clip.media_id))
            .flatten()
            .filter(|media| media.kind == model::MediaKind::Video)
    }

    /// The bin entry behind the one clip selected, when it has cuts to find.
    fn selected_footage(&self) -> Option<model::MediaItem> {
        let id = self.sole_selection()?;
        let clip = self.clip(&id)?;
        self.footage_of(clip).cloned()
    }

    /// Reads the selected clip's file through for its cuts, in the
    /// background, a cut counting where the score reaches `threshold` and
    /// no sooner than `min_seconds` after the last. One read a file at a
    /// time.
    pub fn cuts_detect(&mut self, threshold: f32, min_seconds: f32) {
        let Some(media) = self.selected_footage() else {
            return;
        };
        if self.cut_jobs.contains_key(&media.id) {
            return;
        }
        let settings = Settings {
            threshold: if threshold.is_finite() && threshold > 0.0 {
                f64::from(threshold)
            } else {
                27.0
            },
            min_seconds: if min_seconds.is_finite() {
                f64::from(min_seconds.max(0.0))
            } else {
                0.25
            },
            merge: true,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cut_jobs.insert(media.id.clone(), 0.0);
        self.cut_cancels
            .insert(media.id.clone(), Arc::clone(&cancel));
        log::info!("cuts: reading {} for its cuts", media.name);
        let epoch = crate::host::project_epoch();
        let media_id = media.id.clone();
        spawn_in_project(
            move || {
                let mut last = -1.0f32;
                let reporting = media_id.clone();
                let found = concat_media::scenes::detect(
                    std::path::Path::new(&media.path),
                    &settings,
                    None,
                    &mut |event| {
                        if let Event::Progress { frames, total } = event {
                            let fraction = if total > 0 {
                                frames as f32 / total as f32
                            } else {
                                0.0
                            };
                            // Every percent, not every report.
                            if fraction - last >= 0.01 {
                                last = fraction;
                                let id = reporting.clone();
                                crate::host::on_ui_in_project(epoch, move |studio, _, _| {
                                    if let Some(held) = studio.cut_jobs.get_mut(&id) {
                                        *held = fraction;
                                    }
                                });
                            }
                        }
                    },
                    &cancel,
                );
                (media_id, found)
            },
            |studio, _, _, (media_id, found)| studio.cuts_landed(media_id, found),
        );
    }

    /// A reading's end: the cuts kept on the file; nothing for one
    /// stopped; a word on screen for one that failed.
    fn cuts_landed(
        &mut self,
        media_id: String,
        found: concat_media::Result<Option<concat_media::scenes::Found>>,
    ) {
        self.cut_jobs.remove(&media_id);
        self.cut_cancels.remove(&media_id);
        let found = match found {
            Ok(Some(found)) => found,
            Ok(None) => {
                log::info!("cuts: stopped");
                return;
            }
            Err(error) => {
                log::warn!("cuts: the picture could not be read: {error}");
                self.notify(&tf("cuts.failed", &[&error]), true);
                return;
            }
        };
        let fps = found.fps.max(1.0);
        let times: Vec<f64> = found.cuts.iter().map(|&cut| cut as f64 / fps).collect();
        let count = times.len();
        log::info!("cuts: {count} cuts in {} frames", found.frames);
        self.apply(Command::SetMediaCuts {
            media_id,
            cuts: Some(CutMarks {
                times,
                hidden: false,
            }),
        });
        self.notify(&tf("cuts.found", &[&count]), false);
    }

    /// Stops the reading of the selected clip's file.
    pub fn cuts_cancel(&mut self) {
        let Some(media) = self.selected_footage() else {
            return;
        };
        if let Some(cancel) = self.cut_cancels.get(&media.id) {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Stops every reading: the project is closing.
    pub(super) fn cancel_cuts(&mut self) {
        for cancel in self.cut_cancels.values() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.cut_cancels.clear();
        self.cut_jobs.clear();
    }

    /// Shows the selected clip's cuts on the lanes, or keeps them off -
    /// and out of the snapping.
    pub fn cuts_set_hidden(&mut self, hidden: bool) {
        let Some(media) = self.selected_footage() else {
            return;
        };
        let Some(marks) = media.cuts.as_ref() else {
            return;
        };
        if marks.hidden != hidden {
            self.apply(Command::SetMediaCuts {
                media_id: media.id.clone(),
                cuts: Some(CutMarks {
                    hidden,
                    ..marks.clone()
                }),
            });
        }
    }

    /// Fork: the selected clip, its footage, and the moment of the footage
    /// the playhead shows - with a word on screen when the playhead is
    /// not on the clip.
    fn cut_here(&mut self) -> Option<(Clip, model::MediaItem, f64)> {
        let id = self.sole_selection()?;
        let clip = self.clip(&id)?.clone();
        let media = self.footage_of(&clip)?.clone();
        let playhead = f64::from(self.playhead);
        if playhead < clip.start || playhead >= clip.start + clip.duration {
            self.notify(&t("cuts.notOnClip"), false);
            return None;
        }
        let source = clip.source_start
            + concat_project::speed::source_consumed(&clip, playhead - clip.start);
        Some((clip, media, source))
    }

    /// Fork: takes out the cut the playhead stands on - the nearest within
    /// half a second of it - so the gaps either side of it are one. One
    /// step to undo.
    pub fn cuts_remove_here(&mut self) {
        let Some((clip, media, source)) = self.cut_here() else {
            return;
        };
        let Some(marks) = media.cuts.as_ref() else {
            self.notify(&t("cuts.noneNear"), false);
            return;
        };
        // Half a second of the timeline, in seconds of the footage.
        let span = concat_project::speed::source_consumed(&clip, clip.duration);
        let pace = if clip.duration > 0.0 && span > 0.0 {
            span / clip.duration
        } else {
            1.0
        };
        match concat_project::rhythm::without_cut(marks, source, 0.5 * pace) {
            Some(kept) => {
                self.apply(Command::SetMediaCuts {
                    media_id: media.id.clone(),
                    cuts: Some(kept),
                });
            }
            None => self.notify(&t("cuts.noneNear"), false),
        }
    }

    /// Fork: sets a cut where the playhead stands, on the footage's frame
    /// there - the first of a rhythm begun by hand, if the clip has none
    /// yet. One step to undo.
    pub fn cuts_add_here(&mut self) {
        let Some((_, media, source)) = self.cut_here() else {
            return;
        };
        let rate =
            super::slip::rate_of(&media).unwrap_or_else(|| f64::from(self.frame_rate().max(1.0)));
        let source = (source * rate).round() / rate;
        match concat_project::rhythm::with_cut(media.cuts.as_ref(), source, 0.5 / rate) {
            Some(marks) => {
                self.apply(Command::SetMediaCuts {
                    media_id: media.id.clone(),
                    cuts: Some(marks),
                });
            }
            None => self.notify(&t("cuts.already"), false),
        }
    }

    /// Drops the selected clip's cuts.
    pub fn cuts_clear(&mut self) {
        let Some(media) = self.selected_footage() else {
            return;
        };
        if media.cuts.is_some() {
            self.apply(Command::SetMediaCuts {
                media_id: media.id,
                cuts: None,
            });
        }
    }

    /// What the Cuts panel shows for `clip`.
    pub(super) fn cut_readout(&self, clip: &Clip) -> CutReadout {
        let media = self.footage_of(clip);
        let marks = media.and_then(|media| media.cuts.as_ref());
        CutReadout {
            footage: media.is_some(),
            progress: media
                .and_then(|media| self.cut_jobs.get(&media.id))
                .copied()
                .unwrap_or(-1.0),
            found: marks.is_some(),
            count: marks.map_or(0, |marks| marks.times.len() as i32),
            hidden: marks.is_some_and(|marks| marks.hidden),
        }
    }

    /// The cut lines of every edit on the lanes, in timeline seconds:
    /// what cuts snap to, beside the clips' edges and the beats. The clips
    /// `skip` names - the ones being moved - lend none.
    pub(super) fn cut_targets(&self, skip: impl Fn(&str) -> bool) -> Vec<f32> {
        let mut targets = Vec::new();
        for clip in &self.timeline().clips {
            if skip(&clip.id) {
                continue;
            }
            let Some(media) = self.footage_of(clip) else {
                continue;
            };
            targets.extend(clip_cuts(clip, media).into_iter().map(|at| at as f32));
        }
        targets
    }

    /// An edit's cut lines over its clip on the lanes, as path commands in
    /// a 1x1 box over the whole clip, each a pixel or two wide at the
    /// lanes' zoom; lines closer than a few pixels to the last one drawn
    /// are left out. Empty for a clip without cuts, or with them kept off
    /// the lanes.
    pub(super) fn cut_paths(&self, clip: &Clip) -> SharedString {
        let Some(media) = self.footage_of(clip) else {
            return SharedString::default();
        };
        if media.cuts.as_ref().is_none_or(|marks| marks.hidden) {
            return SharedString::default();
        }
        let duration = clip.duration;
        let seconds_per_pixel = f64::from(self.lanes.seconds_per_pixel);
        if duration <= 0.0 || seconds_per_pixel <= 0.0 {
            return SharedString::default();
        }
        let pixel = seconds_per_pixel / duration;
        let mut path = String::new();
        let mut last = f64::NEG_INFINITY;
        for at in clip_cuts(clip, media) {
            let x = (at - clip.start) / duration;
            if (x - last) / pixel < LINE_GAP {
                continue;
            }
            last = x;
            let (left, right) = (x - pixel, x + pixel);
            let _ = std::fmt::Write::write_fmt(
                &mut path,
                format_args!("M{left:.6} 0L{right:.6} 0L{right:.6} 1L{left:.6} 1Z"),
            );
        }
        path.into()
    }
}
