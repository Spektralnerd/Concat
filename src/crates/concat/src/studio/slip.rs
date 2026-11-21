// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: the Ausschnitt sheet - another stretch of a clip's footage in the
//! same place ([`Command::SlipClip`]). The clip keeps its start and its
//! length, so it still fills the gap it was cut to; a finger along a strip
//! of the film moves the stretch it shows, with the film's own cuts drawn
//! on the strip from its scene list, as a guide and a soft catch, never a
//! wall. Frame and second steps place it exactly, and the monitor shows the
//! clip's picture as it moves.

use std::path::PathBuf;

use concat_host::scenes::{self as files, SceneList};
use concat_project::speed;

use super::*;

/// A slip under the finger: the clip, its in-point when the finger came
/// down, the stretch of film the strip showed then - which holds still
/// under the finger until it lifts, or the scale would change mid-drag -
/// and the gesture that makes the whole drag one step to undo.
pub(super) struct SlipDrag {
    clip_id: String,
    from: f64,
    window: (f64, f64),
    gesture: String,
}

/// The scene list of the film the sheet was opened on, once read: none
/// when the film has none.
pub(super) struct SlipScenes {
    media_id: String,
    list: Option<SceneList>,
}

/// What the Ausschnitt sheet shows for the selected clip.
#[derive(Default)]
pub(super) struct SlipReadout {
    /// The clip is cut from footage it can move through.
    pub footage: bool,
    /// Its first and last frame in the film, as timecodes.
    pub span: String,
    /// Which of the film's scenes it shows, and where a cut falls inside
    /// it, when the film has a scene list.
    pub note: String,
    /// Its stretch on the strip, from the strip's left, `0..=1`.
    pub start: f32,
    pub end: f32,
    /// The film's cuts on the strip: a path in a box 1000 wide and 100
    /// high.
    pub marks: String,
    /// The clip shows a copy of only its own stretch (smooth slow motion
    /// or Enhance), so there is next to nothing to move through.
    pub copy: bool,
}

/// How near a finger brings the stretch's edge to one of the film's cuts
/// for it to catch there, in points of the strip.
const CATCH: f64 = 10.0;

/// A frame rate as a bin entry gives it, exact when it can be: "24000/1001"
/// before 23.976.
pub(super) fn rate_of(media: &model::MediaItem) -> Option<f64> {
    media
        .frame_rate_fraction
        .as_deref()
        .and_then(|fraction| {
            let (num, den) = fraction.split_once('/')?;
            let (num, den) = (
                num.trim().parse::<f64>().ok()?,
                den.trim().parse::<f64>().ok()?,
            );
            (num > 0.0 && den > 0.0).then(|| num / den)
        })
        .or(media.frame_rate)
        .filter(|rate| rate.is_finite() && *rate > 0.0)
}

/// A length in seconds with two decimals, in the interface's own way of
/// writing them.
fn seconds_label(seconds: f64) -> String {
    let number = format!("{seconds:.2}");
    if i18n::current() == "de" {
        number.replace('.', ",")
    } else {
        number
    }
}

impl Studio {
    /// The one clip selected and the footage it is cut from, when it has
    /// footage to move through: a video or a sound from a file.
    fn slip_target(&self) -> Option<(Clip, model::MediaItem)> {
        let id = self.sole_selection()?;
        let clip = self.clip(&id)?;
        if !matches!(clip.kind, model::ClipKind::Video | model::ClipKind::Audio) {
            return None;
        }
        let media = self.project().media_by_id(&clip.media_id)?;
        Some((clip.clone(), media.clone()))
    }

    /// The footage's frame rate: its own, else the timeline's.
    fn slip_rate(&self, media: &model::MediaItem) -> f64 {
        rate_of(media).unwrap_or_else(|| f64::from(self.frame_rate().max(1.0)))
    }

    /// The film's scene list, when it is the one read for this film.
    fn slip_list(&self, media_id: &str) -> Option<&SceneList> {
        self.slip_scenes
            .as_ref()
            .filter(|scenes| scenes.media_id == media_id)
            .and_then(|scenes| scenes.list.as_ref())
    }

    /// The film's cuts, in seconds, rising: its scene list's, else the
    /// cuts found in its picture for cutting to (an edit's own shots).
    fn slip_cuts(&self, media: &model::MediaItem) -> Vec<f64> {
        match self.slip_list(&media.id) {
            Some(list) => list
                .cuts()
                .iter()
                .map(|&frame| list.seconds(frame))
                .collect(),
            None => media
                .cuts
                .as_ref()
                .map_or_else(Vec::new, |marks| marks.times.clone()),
        }
    }

    /// The stretch of film the strip shows around the clip: the scenes it
    /// shows, with as much again either side - at least its own length
    /// and two seconds - so its neighbours are in reach.
    fn slip_window(&self, clip: &Clip, media: &model::MediaItem) -> (f64, f64) {
        let span = speed::source_consumed(clip, clip.duration).max(0.0);
        let (first, last) = (clip.source_start, clip.source_start + span);
        let cuts = self.slip_cuts(media);
        let length = media.duration.filter(|length| *length > 0.0);
        // The scenes the stretch is in: from the cut before its start to
        // the cut after its end.
        let low = cuts
            .iter()
            .copied()
            .rfind(|cut| *cut <= first)
            .unwrap_or(0.0);
        let high = cuts
            .iter()
            .copied()
            .find(|cut| *cut >= last)
            .or(length)
            .unwrap_or(last)
            .max(last);
        let pad = ((high - low) * 0.25).max(span).max(2.0);
        let start = (low - pad).max(0.0);
        let end = match length {
            Some(length) => (high + pad).min(length.max(last)),
            None => high + pad,
        };
        (start, end.max(start + 0.1))
    }

    /// The sheet came up: the film's scene list is read in the background,
    /// and the playhead goes onto the clip's first frame unless it is on
    /// the clip already, so the monitor shows what the slip moves.
    pub fn slip_open(&mut self) {
        let Some((clip, media)) = self.slip_target() else {
            return;
        };
        // Read afresh each time, so a list made since is found; the one
        // held for this film serves until the new one is in.
        let path = PathBuf::from(&media.path);
        let media_id = media.id.clone();
        spawn_in_project(
            move || files::find_list(&path).map(|(_, list)| list),
            move |studio, _, _, list| {
                studio.slip_scenes = Some(SlipScenes { media_id, list });
            },
        );
        let playhead = f64::from(self.playhead);
        if playhead < clip.start || playhead >= clip.start + clip.duration {
            self.seek(clip.start as f32);
        }
    }

    /// A finger came down on the strip.
    pub fn slip_begin(&mut self) {
        let Some((clip, media)) = self.slip_target() else {
            return;
        };
        if self.playing {
            self.pause();
        }
        let window = self.slip_window(&clip, &media);
        self.slip = Some(SlipDrag {
            gesture: format!("slip {} {}", clip.id, self.revision),
            clip_id: clip.id,
            from: clip.source_start,
            window,
        });
    }

    /// The finger has moved `moved` points along a strip `width` points
    /// wide since it came down: the stretch moves with it, later footage to
    /// the right, and catches on a cut of the film it comes near.
    pub fn slip_drag(&mut self, moved: f32, width: f32) {
        let Some(drag) = self.slip.as_ref() else {
            return;
        };
        let Some(clip) = self.clip(&drag.clip_id) else {
            return;
        };
        let Some(media) = self.project().media_by_id(&clip.media_id) else {
            return;
        };
        if !(moved.is_finite() && width.is_finite() && width > 1.0) {
            return;
        }
        let per_point = (drag.window.1 - drag.window.0) / f64::from(width);
        // On a frame of the footage, so the clip starts on a whole picture.
        let rate = self.slip_rate(media);
        let mut target = ((drag.from + f64::from(moved) * per_point) * rate).round() / rate;
        let span = speed::source_consumed(clip, clip.duration).max(0.0);
        let reach = CATCH * per_point;
        let caught = self
            .slip_cuts(media)
            .into_iter()
            .flat_map(|cut| [cut, cut - span])
            .map(|at| (at, (at - target).abs()))
            .filter(|(_, off)| *off <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((at, _)) = caught {
            target = at;
        }
        let (clip_id, gesture) = (drag.clip_id.clone(), drag.gesture.clone());
        self.apply_in(
            Some(&gesture),
            Command::SlipClip {
                clip_id,
                source_start: target,
            },
        );
    }

    /// The finger lifted.
    pub fn slip_end(&mut self) {
        self.slip = None;
    }

    /// Moves the stretch by `frames` of the footage and `seconds`: a step
    /// of its own to undo, or part of the one a held button makes.
    pub fn slip_nudge(&mut self, frames: i32, seconds: f32) {
        let Some((clip, media)) = self.slip_target() else {
            return;
        };
        if self.playing {
            self.pause();
        }
        let rate = self.slip_rate(&media);
        let by = f64::from(frames) / rate + f64::from(seconds);
        if !by.is_finite() || by == 0.0 {
            return;
        }
        // Onto the footage's frames, from wherever a drag left it.
        let source_start = ((clip.source_start + by) * rate).round() / rate;
        let gesture = self
            .slip
            .as_ref()
            .filter(|drag| drag.clip_id == clip.id)
            .map(|drag| drag.gesture.clone());
        self.apply_in(
            gesture.as_deref(),
            Command::SlipClip {
                clip_id: clip.id,
                source_start,
            },
        );
    }

    /// Shows the clip's first frame (0), its last (1), or plays it once
    /// from its start to its end (2) - pausing it when it is playing.
    pub fn slip_look(&mut self, what: i32) {
        let Some((clip, _)) = self.slip_target() else {
            return;
        };
        let frame = 1.0 / f64::from(self.frame_rate().max(1.0));
        let last = (clip.start + clip.duration - frame).max(clip.start);
        if what != 2 && self.playing {
            self.pause();
        }
        match what {
            0 => self.seek(clip.start as f32),
            1 => self.seek(last as f32),
            _ => {
                if self.playing {
                    self.pause();
                    return;
                }
                self.seek(clip.start as f32);
                self.play_until = Some(last as f32);
                self.play_toggle();
                if !self.playing {
                    self.play_until = None;
                }
            }
        }
    }

    /// What the Ausschnitt sheet shows for `clip`.
    pub(super) fn slip_readout(&self, clip: &Clip) -> SlipReadout {
        let mut readout = SlipReadout::default();
        if !matches!(clip.kind, model::ClipKind::Video | model::ClipKind::Audio) {
            return readout;
        }
        let Some(media) = self.project().media_by_id(&clip.media_id) else {
            return readout;
        };
        readout.footage = true;
        readout.copy = clip.smooth.as_ref().is_some_and(|smooth| smooth.on)
            || clip.enhanced.as_ref().is_some_and(|enhanced| enhanced.on);
        let rate = self.slip_rate(media);
        let span = speed::source_consumed(clip, clip.duration).max(0.0);
        let (first, end) = (clip.source_start, clip.source_start + span);
        let last = (end - 1.0 / rate).max(first);
        let timecode = |seconds: f64| {
            // A hair past the moment, so a frame's own time never reads as
            // the one before it.
            crate::format::frames_timecode((seconds + 1e-4) as f32, rate as f32)
        };
        readout.span = format!("{} – {}", timecode(first), timecode(last));

        let window = match self.slip.as_ref() {
            Some(drag) if drag.clip_id == clip.id => drag.window,
            _ => self.slip_window(clip, media),
        };
        let width = (window.1 - window.0).max(1e-6);
        let place = |seconds: f64| ((seconds - window.0) / width).clamp(0.0, 1.0) as f32;
        readout.start = place(first);
        readout.end = place(end);

        let cuts = self.slip_cuts(media);
        if cuts.is_empty() {
            return readout;
        }
        readout.marks = cuts
            .iter()
            .filter(|cut| **cut > window.0 && **cut < window.1)
            .map(|cut| {
                let x = (cut - window.0) / width * 1000.0;
                format!("M {x:.1} 0 L {x:.1} 100 ")
            })
            .collect();
        // Scenes are counted from one, as the Scenes sheet numbers them.
        let scene_at = |seconds: f64| cuts.partition_point(|cut| *cut <= seconds) + 1;
        let tolerance = 0.5 / rate;
        let (opening, closing) = (scene_at(first + tolerance), scene_at(end - tolerance));
        readout.note = if closing <= opening {
            tf("slip.inScene", &[&opening])
        } else {
            // Where the first cut inside it falls, on the timeline.
            let cut = cuts
                .iter()
                .copied()
                .find(|cut| *cut > first + tolerance)
                .unwrap_or(first);
            let pace = if span > 0.0 && clip.duration > 0.0 {
                span / clip.duration
            } else {
                1.0
            };
            tf(
                "slip.acrossScenes",
                &[&opening, &closing, &seconds_label((cut - first) / pace)],
            )
        };
        readout
    }
}
