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

    /// Makes the smooth copy of the span clip `id` shows, and points the
    /// clip at it when it is whole and the section still ticked.
    pub fn smooth_clip(&mut self, id: &str) {
        if !self.smooth_jobs.is_empty() || self.host.tweeners.is_busy() {
            self.notify(&t("studio.smoothAlreadyWorkOne"), true);
            return;
        }
        let Some(clip) = self.clip(id).cloned() else {
            return;
        };
        if clip.kind != model::ClipKind::Video {
            return;
        }
        let Some(media) = self.smooth_source(&clip) else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let project = std::path::PathBuf::from(session.path());
        if media.width.unwrap_or(0) == 0 || media.height.unwrap_or(0) == 0 {
            self.notify(&t("studio.fileHasNoPicture"), true);
            return;
        }
        let fps = media.frame_rate.unwrap_or(30.0);
        let factor = host_tween::effective_factor(self.smooth_factor, fps);
        let method = self.smooth_method.with_quick(self.smooth_quick);
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
            return;
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
            },
        );
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
