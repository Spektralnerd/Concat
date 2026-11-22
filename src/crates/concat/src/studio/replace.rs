// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: Replace - another scene in a selected clip's place
//! ([`Command::ReplaceClipSource`]). The clip's Replace tool opens the bin
//! for it; the file or the film's scene picked there takes the clip's
//! start and length - a longer one its start, middle or end, as the Scenes
//! sheet's rhythm says, a shorter one slowed to fill it if asked - and
//! keeps its looks, so nothing after it moves.

use concat_project::rhythm::{self, Part};

use super::*;

impl Studio {
    /// The Replace tool: the clip a pick from the bin or the Scenes sheet
    /// goes into.
    pub fn replace_begin(&mut self) {
        self.replace_target = self.sole_selection();
    }

    /// The bin closed without a pick.
    pub fn replace_end(&mut self) {
        self.replace_target = None;
    }

    /// Puts `media_id` into the clip Replace was opened for: from `first`
    /// seconds into it for `length` seconds - a scene of a film - or the
    /// whole file, cut to the clip's length. The clip stays selected and
    /// the monitor shows its first frame.
    pub(crate) fn replace_with(&mut self, media_id: &str, first: f64, length: Option<f64>) {
        let Some(target) = self.replace_target.take() else {
            self.notify(&t("replace.noClip"), true);
            return;
        };
        let Some(clip) = self.clip(&target).cloned() else {
            self.notify(&t("replace.noClip"), true);
            return;
        };
        let Some(media) = self.project().media_by_id(media_id).cloned() else {
            return;
        };
        let room = clip.duration;
        let (source_start, duration, speed) = if media.kind == model::MediaKind::Image {
            (0.0, room, 1.0)
        } else {
            let length = length
                .or_else(|| media.duration.map(|whole| whole - first))
                .unwrap_or(room)
                .max(0.0);
            let (part, slow) = super::scenes::fill_knobs();
            let fit = rhythm::fit(first, length, room, Part::from_index(part), slow);
            (fit.source_start, fit.duration, fit.speed.unwrap_or(1.0))
        };
        self.close_source();
        log::info!("replace: {} now plays {}", clip.name, media.name);
        self.apply(Command::ReplaceClipSource {
            clip_id: target.clone(),
            media_id: media.id.clone(),
            source_start,
            duration,
            speed,
        });
        self.selection = vec![target];
        self.seek(clip.start as f32);
    }
}
