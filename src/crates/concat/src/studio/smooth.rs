// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Smooth slow motion: a clip pointed at a copy of its span with frames
//! made in between its own (`concat_host::tween`), and back again.
//!
//! The section works as Enhance's does. Its tick turns it on and, on a
//! clip that has a copy, switches between the copy and the original, one
//! undo step; its button makes a copy from the original with the section's
//! Frames and Method, or stops the one being made. The copy covers the
//! span the clip shows of its original and a second either side, so the
//! clip's in-point, length, speed and curve are left as they were.

use concat_host::tween::{self as host_tween, Method, TweenRequest};
use concat_project::speed;

use super::*;

/// The section's rows for one clip; see [`Studio::smooth_readout`].
pub(super) struct SmoothReadout {
    /// The clip is footage: the section has something to work on.
    pub footage: bool,
    /// The factor the next copy is made at for this clip's file, after the
    /// rate cap (see `concat_host::tween::effective_factor`).
    pub factor: i32,
    /// What the clip's copy, if it has one, was made with: "4× · AI".
    pub made: String,
    /// The frames a second the clip's file has and the copy would have.
    pub rates: String,
}

/// Fork: the queue the section's "All slowed clips" lines up - every
/// slowed clip on the timeline made smooth one after another, each with as
/// many frames as its slowest stretch wants, by the method the section had
/// when it was asked. One run at a time still: the queue starts the next
/// when the last is through, however it ended.
pub(super) struct SmoothQueue {
    /// Clips still to make, in timeline order, with their frames.
    pending: std::collections::VecDeque<(String, u32)>,
    /// The clip being made now.
    current: Option<String>,
    /// How many were lined up, how many are through - made, failed or
    /// stopped by hand - and how many of those failed.
    total: usize,
    through: usize,
    failed: usize,
    method: Method,
    /// Clips whose tick the queue put on itself, without a copy before:
    /// taken off again for any it did not make.
    ticked_here: HashSet<String>,
    /// The phone's screen kept on from the first clip to the last. Each run
    /// holds it too, but only while it runs: between two the screen would
    /// time out and the rest wait in the dark.
    _awake: concat_host::awake::Awake,
}

/// Fork: the queue's line for the section and the phone's top line.
pub(super) struct SmoothQueueReadout {
    pub active: bool,
    /// "Smooth slow motion 3 of 41 · 7 %", empty while no queue runs.
    pub line: String,
    /// The whole queue's share done, the clip in hand counted by its part.
    pub progress: f32,
}

impl Studio {
    /// The Smooth slow motion tick in the inspector, for the one selected
    /// clip. It never starts a run; ticked, the section's settings and
    /// button come alive. On a clip with a copy it also switches the clip
    /// between the copy and its original, one undo step. Unticked means the
    /// clip shows its original, so a run going for it is stopped.
    pub fn clip_smooth_set(&mut self, on: bool) {
        let Some(id) = self.sole_selection() else {
            return;
        };
        let Some(clip) = self.clip(&id) else {
            return;
        };
        if self.locked(&clip.track_id) {
            return;
        }
        let linked = clip.smooth.is_some();
        if !on && self.smooth_jobs.contains_key(&id) {
            self.host.tweeners.cancel();
        }
        if on {
            self.smooth_ticked.insert(id.clone());
        } else {
            self.smooth_ticked.remove(&id);
        }
        if linked {
            self.apply(Command::SetClipSmooth { clip_id: id, on });
            self.request_preview();
        }
    }

    /// Whether clip `clip`'s section is ticked: its copy shown, or, without
    /// a copy, the tick put on in this window.
    pub(super) fn smooth_ticked(&self, clip: &Clip) -> bool {
        match clip.smooth.as_ref() {
            Some(link) => link.on,
            None => self.smooth_ticked.contains(&clip.id),
        }
    }

    /// The Frames row: two to thirty-two frames for each of the file's.
    pub fn clip_smooth_factor(&mut self, factor: i32) {
        self.smooth_factor = host_tween::factor_of(factor.max(0) as u32);
    }

    /// The Method row, by its place in [`Method::ALL`].
    pub fn clip_smooth_method(&mut self, index: i32) {
        if let Some(method) = Method::ALL.get(index.max(0) as usize) {
            self.smooth_method = *method;
        }
    }

    /// Faster, under AI: the motion read at half size, two to three times
    /// as quick. Kept when another method is picked and back again.
    pub fn clip_smooth_quick(&mut self, on: bool) {
        self.smooth_quick = on;
    }

    /// The section's one button: Make smooth, or again on a clip that has
    /// a copy - from the original, with the settings as they are now, a
    /// copy those settings made before picked up at once - and Stop while
    /// this clip's run is going. Only on a ticked section, as the button is.
    pub fn clip_smooth_run(&mut self) {
        let Some(id) = self.sole_selection() else {
            return;
        };
        if self.smooth_jobs.contains_key(&id) {
            self.host.tweeners.cancel();
            return;
        }
        if self
            .clip(&id)
            .is_some_and(|clip| !self.locked(&clip.track_id) && self.smooth_ticked(clip))
        {
            self.smooth_clip(&id);
        }
    }

    /// The file a copy is made from for `clip`: its original when it shows,
    /// or holds, a copy already - a second run starts from the source, not
    /// from the first run's frames.
    pub(super) fn smooth_source(&self, clip: &Clip) -> Option<model::MediaItem> {
        let id = clip
            .smooth
            .as_ref()
            .map_or(&clip.media_id, |link| &link.original);
        self.project()
            .media
            .iter()
            .find(|item| &item.id == id)
            .cloned()
    }

    /// Clip `clip`'s in-point in its original's seconds: its own, or, while
    /// it shows a smooth copy, its own moved by where that copy starts.
    fn smooth_original_start(clip: &Clip) -> f64 {
        match &clip.smooth {
            Some(link) if link.on => clip.source_start + link.offset,
            _ => clip.source_start,
        }
    }

    /// Makes the smooth copy of the span clip `id` shows, with the
    /// section's Frames and Method, and points the clip at it when it is
    /// whole and the section still ticked.
    pub fn smooth_clip(&mut self, id: &str) {
        let method = self.smooth_method.with_quick(self.smooth_quick);
        self.smooth_clip_with(id, self.smooth_factor, method);
    }

    /// [`Self::smooth_clip`] with the frames asked and the method given:
    /// the section's for its button, a clip's own for the queue. Whether a
    /// run started.
    fn smooth_clip_with(&mut self, id: &str, asked: u32, method: Method) -> bool {
        if !self.smooth_jobs.is_empty() || self.host.tweeners.is_busy() {
            self.notify(&t("studio.smoothAlreadyWorkOne"), true);
            return false;
        }
        let Some(clip) = self.clip(id).cloned() else {
            return false;
        };
        if clip.kind != model::ClipKind::Video {
            return false;
        }
        let Some(media) = self.smooth_source(&clip) else {
            return false;
        };
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let project = std::path::PathBuf::from(session.path());
        if media.width.unwrap_or(0) == 0 || media.height.unwrap_or(0) == 0 {
            self.notify(&t("studio.fileHasNoPicture"), true);
            return false;
        }
        let fps = media.frame_rate.unwrap_or(30.0);
        let factor = host_tween::effective_factor(asked, fps);
        // The span the clip shows of the original, and a second either side
        // so a small trim after is still inside the copy.
        let start = Self::smooth_original_start(&clip);
        let covered = clip.duration * clip.speed;
        let from = (start - SMOOTH_MARGIN).max(0.0);
        let to = (start + covered + SMOOTH_MARGIN).min(media.duration.unwrap_or(f64::MAX));
        let span = (from, (to - from).max(covered));
        let Some(target) = host_tween::target_for(&project, &media.path, factor, method, span)
        else {
            self.notify(&tf("studio.couldNotRead", &[&media.path]), true);
            return false;
        };
        let request = TweenRequest {
            media_path: media.path.clone(),
            factor,
            method,
            span,
            target,
        };
        log::info!(
            "smooth: making {} {factor}x by {}, {:.2}s from {:.2}s",
            media.name,
            method.name(),
            span.1,
            span.0
        );
        let clip_id = clip.id.clone();
        self.smooth_jobs.insert(clip_id.clone(), (false, 0.0));
        self.notify(
            &tf(
                "studio.smoothingFrames",
                &[&media.name, &factor.to_string()],
            ),
            false,
        );
        let tweeners = Arc::clone(&self.host.tweeners);
        let epoch = crate::host::project_epoch();
        spawn_in_project(
            move || {
                let mut last = (false, -1.0f32);
                let reporting = clip_id.clone();
                let result = tweeners.tween(&request, &mut |progress| {
                    let now = match progress {
                        host_tween::Progress::Fetching { received, total } => {
                            (true, received as f32 / total.max(1) as f32)
                        }
                        host_tween::Progress::Analysing(fraction) => (false, fraction),
                    };
                    if now.0 != last.0 || now.1 - last.1 >= 0.01 {
                        last = now;
                        let id = reporting.clone();
                        on_ui_in_project(epoch, move |studio, _, _| {
                            if let Some(held) = studio.smooth_jobs.get_mut(&id) {
                                *held = now;
                            }
                        });
                    }
                });
                (clip_id, result)
            },
            move |studio, _, _, (clip_id, result)| {
                studio.smooth_jobs.remove(&clip_id);
                // Fork: what the queue counts - a stop by hand is not a
                // failure, only an error is.
                let failed = matches!(&result, Err(error) if !error.contains("cancelled"));
                // A cancelled or failed run leaves the clip as it was, and so
                // does one whose tick went off as it finished: the copy stays
                // in the cache, and ticking and running again find it.
                let ticked = studio
                    .clip(&clip_id)
                    .is_some_and(|clip| studio.smooth_ticked(clip));
                // Each ending says so in the log, which is all there is to go
                // on when a phone's run comes back and nothing changes.
                match result {
                    Ok(path) if ticked => {
                        studio.smooth_ticked.remove(&clip_id);
                        studio.adopt_smooth(&clip_id, &path, factor, method, span.0);
                    }
                    Ok(path) => log::info!(
                        "smooth: {} is made, but its clip is {}; it waits in the cache",
                        path.display(),
                        if studio.clip(&clip_id).is_some() {
                            "no longer ticked"
                        } else {
                            "gone"
                        }
                    ),
                    Err(error) if error.contains("cancelled") => {
                        log::info!("smooth: stopped");
                    }
                    Err(error) => {
                        log::warn!("smooth: {error}");
                        studio.notify(&tf("studio.smooth", &[&error]), true);
                    }
                }
                // Fork: the queue, if this was its clip, goes on to the next.
                studio.smooth_queue_after(&clip_id, failed);
            },
        );
        true
    }

    /// Points clip `id` at the smooth copy at `path`, which starts `offset`
    /// into the original, probed the way an import is, and shows it - unless
    /// the clip has moved off the copy's span meanwhile.
    fn adopt_smooth(
        &mut self,
        id: &str,
        path: &std::path::Path,
        factor: u32,
        method: Method,
        offset: f64,
    ) {
        let Some(clip) = self.clip(id).cloned() else {
            return;
        };
        let start = Self::smooth_original_start(&clip) - offset;
        if start < -1e-6 {
            log::warn!(
                "smooth: the clip now starts {:.3}s before its copy does; not shown",
                -start
            );
            self.notify(&t("studio.smoothClipMoved"), true);
            return;
        }
        let summary = match media::probe(&path.to_string_lossy()) {
            Ok(summary) => summary,
            Err(error) => {
                log::warn!("smooth: {} could not be read: {error}", path.display());
                self.notify(&tf("studio.smooth", &[&error]), true);
                return;
            }
        };
        let mut item = summary.to_new_media();
        // The bin names the copy after the original and what it was made
        // with - one file can have a copy for each setting.
        if let Some(original) = self.smooth_source(&clip) {
            item.name = format!(
                "{} (smooth {factor}x, {})",
                original.name,
                method_label(method)
            );
        }
        item.origin = Some(model::MediaOrigin::Processed);
        if let Some(length) = item.duration
            && start + clip.duration * clip.speed > length + 0.05
        {
            log::warn!(
                "smooth: the clip needs {:.3}s of its copy from {start:.3}s, and the copy holds {length:.3}s; not shown",
                clip.duration * clip.speed
            );
            self.notify(&t("studio.smoothClipMoved"), true);
            return;
        }
        self.apply(Command::SmoothClip {
            clip_id: id.to_owned(),
            item,
            source_start: Some(start.max(0.0)),
            offset,
            factor,
            method: method.name().to_owned(),
        });
        self.request_preview();
        self.notify(&t("studio.smoothClipNowShows"), false);
    }

    /// The section's readout for `clip`: what it is, what its next copy
    /// would be, and what its copy was made with.
    pub(super) fn smooth_readout(&self, clip: &Clip) -> SmoothReadout {
        let footage = clip.kind == model::ClipKind::Video;
        let fps = self
            .smooth_source(clip)
            .and_then(|media| media.frame_rate)
            .unwrap_or(0.0);
        let factor = host_tween::effective_factor(self.smooth_factor, fps.max(1.0));
        let made = clip.smooth.as_ref().map_or(String::new(), |link| {
            format!(
                "{}× · {}",
                link.factor,
                method_label(Method::parse(&link.method))
            )
        });
        let rates = if fps > 0.0 {
            format!(
                "{} → {} fps",
                rate_label(fps),
                rate_label(fps * f64::from(factor))
            )
        } else {
            String::new()
        };
        SmoothReadout {
            footage,
            factor: factor as i32,
            made,
            rates,
        }
    }

    /// Fork: the section's "All slowed clips". Lines up every clip on the
    /// timeline - or of the selection, when it holds more than one -
    /// slowed enough to want frames between its own
    /// ([`speed::smooth_frames_for`]) that has no copy made that way yet,
    /// and makes them one after another, in timeline order. Left out: clips
    /// on a hidden or locked lane - a template's guide edit, say - and
    /// clips switched back to their original by hand. The frames are each
    /// clip's own, for its slowest stretch at the timeline's rate; the
    /// method is the section's.
    pub fn clip_smooth_queue_all(&mut self) {
        if self.smooth_queue.is_some() {
            return;
        }
        if !self.smooth_jobs.is_empty() || self.host.tweeners.is_busy() {
            self.notify(&t("studio.smoothAlreadyWorkOne"), true);
            return;
        }
        let method = self.smooth_method.with_quick(self.smooth_quick);
        let timeline_rate = f64::from(self.frame_rate());
        let timeline = self.timeline();
        let hidden: HashSet<&str> = timeline
            .tracks
            .iter()
            .filter(|track| !track.visible)
            .map(|track| track.id.as_str())
            .collect();
        let chosen: Option<HashSet<&str>> = (self.selection.len() >= 2)
            .then(|| self.selection.iter().map(String::as_str).collect());
        let mut lined: Vec<(f64, String, u32)> = Vec::new();
        for clip in &timeline.clips {
            if clip.kind != model::ClipKind::Video
                || chosen.as_ref().is_some_and(|ids| !ids.contains(clip.id.as_str()))
                || hidden.contains(clip.track_id.as_str())
                || self.locked(&clip.track_id)
                || clip.smooth.as_ref().is_some_and(|link| !link.on)
            {
                continue;
            }
            let Some(media) = self.smooth_source(clip) else {
                continue;
            };
            if media.width.unwrap_or(0) == 0 || media.height.unwrap_or(0) == 0 {
                continue;
            }
            // A file that does not say is taken at 30, as a single run does.
            let file_rate = media.frame_rate.unwrap_or(30.0);
            let Some(frames) = speed::smooth_frames_for(
                timeline_rate,
                file_rate,
                speed::slowest_speed(clip),
                &host_tween::FACTORS,
            ) else {
                continue;
            };
            let frames = host_tween::effective_factor(frames, file_rate);
            // A copy as good is made already: as many frames or more, by the
            // same method - or by AI in full where AI faster is asked.
            let made = clip.smooth.as_ref().is_some_and(|link| {
                let by = Method::parse(&link.method);
                link.on
                    && link.factor >= frames
                    && (by == method || (by == Method::Motion && method == Method::MotionQuick))
            });
            if !made {
                lined.push((clip.start, clip.id.clone(), frames));
            }
        }
        if lined.is_empty() {
            self.notify(&t("studio.smoothQueueNone"), false);
            return;
        }
        lined.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut ticked_here = HashSet::new();
        for (_, id, _) in &lined {
            let unlinked = self.clip(id).is_some_and(|clip| clip.smooth.is_none());
            if unlinked && self.smooth_ticked.insert(id.clone()) {
                ticked_here.insert(id.clone());
            }
        }
        let total = lined.len();
        log::info!(
            "smooth: queue of {total} clips by {}, frames {:?}",
            method.name(),
            lined.iter().map(|(_, _, frames)| *frames).collect::<Vec<_>>()
        );
        self.smooth_queue = Some(SmoothQueue {
            pending: lined.into_iter().map(|(_, id, frames)| (id, frames)).collect(),
            current: None,
            total,
            through: 0,
            failed: 0,
            method,
            ticked_here,
            _awake: concat_host::awake::Awake::hold(),
        });
        self.notify(&tf("studio.smoothQueueStarted", &[&total]), false);
        self.smooth_queue_next();
    }

    /// Fork: the queue's Stop - the clip being made, and the rest after it.
    /// What is made stays made.
    pub fn clip_smooth_queue_stop(&mut self) {
        let Some(queue) = self.smooth_queue.take() else {
            return;
        };
        if queue.current.is_some() {
            self.host.tweeners.cancel();
        }
        log::info!(
            "smooth: queue stopped after {} of {}",
            queue.through,
            queue.total
        );
        self.untick_unmade(&queue.ticked_here);
        self.notify(
            &tf(
                "studio.smoothQueueStopped",
                &[&queue.through, &queue.total],
            ),
            false,
        );
    }

    /// Fork: starts the queue's next clip, past any that are gone or will
    /// not start; with none left the queue is done.
    fn smooth_queue_next(&mut self) {
        loop {
            let Some(queue) = self.smooth_queue.as_mut() else {
                return;
            };
            let Some((id, frames)) = queue.pending.pop_front() else {
                self.smooth_queue_finish();
                return;
            };
            let method = queue.method;
            queue.current = Some(id.clone());
            if self.smooth_clip_with(&id, frames, method) {
                return;
            }
            // It did not start: through, and a failure if the clip is still
            // there - one deleted meanwhile is simply not made.
            let exists = self.clip(&id).is_some();
            if let Some(queue) = self.smooth_queue.as_mut() {
                queue.current = None;
                queue.through += 1;
                if exists {
                    queue.failed += 1;
                }
            }
        }
    }

    /// Fork: a run ended; when it was the queue's, the queue counts it and
    /// starts the next. A run started from a clip's own button while no
    /// queue runs passes by.
    fn smooth_queue_after(&mut self, id: &str, failed: bool) {
        let Some(queue) = self.smooth_queue.as_mut() else {
            return;
        };
        if queue.current.as_deref() != Some(id) {
            return;
        }
        queue.current = None;
        queue.through += 1;
        if failed {
            queue.failed += 1;
        }
        self.smooth_queue_next();
    }

    /// Fork: the queue is through: says how it went and lets the screen
    /// rest again.
    fn smooth_queue_finish(&mut self) {
        let Some(queue) = self.smooth_queue.take() else {
            return;
        };
        let made = queue.through.saturating_sub(queue.failed);
        log::info!(
            "smooth: queue done, {made} of {} made, {} failed",
            queue.total,
            queue.failed
        );
        self.untick_unmade(&queue.ticked_here);
        if queue.failed > 0 {
            self.notify(
                &tf("studio.smoothQueueDoneFailed", &[&made, &queue.failed]),
                true,
            );
        } else {
            self.notify(&tf("studio.smoothQueueDone", &[&made]), false);
        }
        self.request_preview();
    }

    /// Fork: takes the tick off again for each clip the queue ticked that
    /// still has no copy: its section reads as it did before.
    fn untick_unmade(&mut self, ticked_here: &HashSet<String>) {
        for id in ticked_here {
            if self.clip(id).is_none_or(|clip| clip.smooth.is_none()) {
                self.smooth_ticked.remove(id);
            }
        }
    }

    /// Fork: the queue's line and share done, for the section and the
    /// phone's top line.
    pub(super) fn smooth_queue_readout(&self) -> SmoothQueueReadout {
        let Some(queue) = self.smooth_queue.as_ref() else {
            return SmoothQueueReadout {
                active: false,
                line: String::new(),
                progress: 0.0,
            };
        };
        let part = queue
            .current
            .as_ref()
            .and_then(|id| self.smooth_jobs.get(id))
            .map_or(0.0, |(fetching, fraction)| if *fetching { 0.0 } else { *fraction });
        let progress =
            ((queue.through as f32 + part) / queue.total.max(1) as f32).clamp(0.0, 1.0);
        let place = (queue.through + 1).min(queue.total);
        let percent = (progress * 100.0).floor() as i32;
        SmoothQueueReadout {
            active: true,
            line: tf("smooth.queueLine", &[&place, &queue.total, &percent]),
            progress,
        }
    }
}

/// A method's short name on a label, as the Method row shows it.
fn method_label(method: Method) -> String {
    match method {
        Method::Motion => t("smooth.methodAi"),
        Method::MotionQuick => t("smooth.methodAiQuick"),
        Method::Blend => t("smooth.methodBlend"),
        Method::Classic => t("smooth.methodClassic"),
    }
}

/// Frames a second as a label: whole where it is whole, else to two places.
fn rate_label(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.005 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    }
}
