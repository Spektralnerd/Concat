// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The beat grid: the beats found in a file's sound, kept on its bin entry
//! (see [`MediaItem::beats`]), and how the lanes count them - every beat a
//! line to cut on, or every bar, or every second bar.
//!
//! The beats are moments of the file, so a clip of it split, trimmed or
//! slowed still plays them where they are in the music: [`clip_beats`] puts
//! them on the timeline through the clip's in-point and speed. Finding them
//! is `concat_media::beats`; this is only what is kept and the counting.

use serde::{Deserialize, Serialize};

use crate::model::{Clip, ClipKind, MediaItem};
use crate::speed;

/// What a beat is on the lanes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BeatMark {
    /// A line of the grid: drawn across the clip, and what cuts snap to.
    Line,
    /// The first beat of a bar that is not a line of the grid.
    Bar,
    /// Any other beat.
    Beat,
}

/// The beats of a file's sound and the grid counted on them.
///
/// Every field is the person's to change except what was found (`tempo`,
/// `beats`, `accents`, `confidence`), which a new detection replaces.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BeatGrid {
    /// Beats per minute, as found: for show, and to set Beat Shake by.
    pub tempo: f64,
    /// When each beat falls, in seconds into the file, in order.
    #[serde(deserialize_with = "crate::model::wire::list")]
    pub beats: Vec<f64>,
    /// How much each beat sounds like the first of a bar, one per beat; what
    /// [`BeatGrid::count_in`] picks the bar's start by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "crate::model::wire::list")]
    pub accents: Vec<f32>,
    /// How sure the finder was, 0 to 1: the share of the beats that sit on
    /// a hit in the sound.
    pub confidence: f32,
    /// Beats in a bar.
    pub per_bar: u32,
    /// The beat the counting starts from, by its index in `beats`: a bar's
    /// first beat and a line of the grid. Kept within one turn of the grid,
    /// `0..per_bar * every`.
    pub first: i64,
    /// How many beats, or bars, from one line of the grid to the next.
    pub every: u32,
    /// Whether `every` counts bars rather than beats.
    pub bars: bool,
    /// Seconds every beat is moved by, earlier when negative: for a cut
    /// that should come a frame ahead of the hit, or a file whose sound is
    /// late.
    pub offset: f64,
    /// The lines kept off the lanes and out of snapping; the beats are kept.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Default for BeatGrid {
    /// Nothing found yet, counted in bars of four, a line every bar.
    fn default() -> Self {
        BeatGrid {
            tempo: 0.0,
            beats: Vec::new(),
            accents: Vec::new(),
            confidence: 0.0,
            per_bar: 4,
            first: 0,
            every: 1,
            bars: true,
            offset: 0.0,
            hidden: false,
        }
    }
}

impl BeatGrid {
    /// The most beats a bar, and beats or bars from line to line, are held
    /// to: far past any music, short of a number that loses the grid.
    pub const MOST: u32 = 64;

    /// A grid on beats just found: counted in bars of `per_bar` from the
    /// beat the accents put first, a line every `every` beats or bars.
    pub fn found(
        tempo: f64,
        beats: Vec<f64>,
        accents: Vec<f32>,
        confidence: f32,
        per_bar: u32,
        every: u32,
        bars: bool,
    ) -> BeatGrid {
        let mut grid = BeatGrid {
            tempo,
            beats,
            accents,
            confidence,
            every,
            bars,
            ..BeatGrid::default()
        };
        grid.count_in(per_bar);
        grid
    }

    /// Beats from one line of the grid to the next.
    pub fn cycle(&self) -> i64 {
        let every = i64::from(self.every.max(1));
        if self.bars {
            every * i64::from(self.per_bar.max(1))
        } else {
            every
        }
    }

    /// What beat `index` is.
    pub fn mark(&self, index: usize) -> BeatMark {
        let from = index as i64 - self.first;
        if from.rem_euclid(self.cycle()) == 0 {
            BeatMark::Line
        } else if from.rem_euclid(i64::from(self.per_bar.max(1))) == 0 {
            BeatMark::Bar
        } else {
            BeatMark::Beat
        }
    }

    /// Every beat as a moment of the file, the offset taken, with what it
    /// is.
    pub fn marks(&self) -> impl Iterator<Item = (f64, BeatMark)> + '_ {
        self.beats
            .iter()
            .enumerate()
            .map(|(index, beat)| (beat + self.offset, self.mark(index)))
    }

    /// Counts in bars of `per_bar` beats, the bar starting on the beat the
    /// accents say: what changing the time signature does.
    pub fn count_in(&mut self, per_bar: u32) {
        self.per_bar = per_bar.clamp(1, Self::MOST);
        self.first = concat_core::beats::downbeat(&self.accents, self.per_bar as usize) as i64;
        self.tidy();
    }

    /// Moves the counting on by `beats` beats, back when negative: the bar
    /// started a beat late, or the second bar of each pair is the one to cut
    /// on.
    pub fn shift(&mut self, beats: i64) {
        self.first += beats;
        self.tidy();
    }

    /// A beat between every two: the finder counted half the tempo.
    pub fn doubled(&self) -> BeatGrid {
        let mut beats = Vec::with_capacity(self.beats.len() * 2);
        let mut accents = Vec::with_capacity(self.beats.len() * 2);
        // The beats put between are never a bar's start before the beats
        // that were found.
        let below = self.accents.iter().cloned().fold(0.0f32, f32::min) - 1.0;
        for (index, beat) in self.beats.iter().enumerate() {
            beats.push(*beat);
            accents.push(self.accents.get(index).copied().unwrap_or(0.0));
            if let Some(next) = self.beats.get(index + 1) {
                beats.push((beat + next) / 2.0);
                accents.push(below);
            }
        }
        let mut grid = BeatGrid {
            tempo: self.tempo * 2.0,
            beats,
            accents,
            first: self.first * 2,
            ..self.clone()
        };
        grid.tidy();
        grid
    }

    /// Every other beat, keeping the one the counting starts from: the
    /// finder counted twice the tempo.
    pub fn halved(&self) -> BeatGrid {
        let keep = self.first.rem_euclid(2) as usize;
        let beats: Vec<f64> = self.beats.iter().skip(keep).step_by(2).copied().collect();
        let accents: Vec<f32> = self.accents.iter().skip(keep).step_by(2).copied().collect();
        let mut grid = BeatGrid {
            tempo: self.tempo / 2.0,
            beats,
            accents,
            first: (self.first - keep as i64) / 2,
            ..self.clone()
        };
        grid.tidy();
        grid
    }

    /// Every field held to what the grid can count with: whatever a
    /// document or a caller hands over.
    pub fn tidy(&mut self) {
        self.per_bar = self.per_bar.clamp(1, Self::MOST);
        self.every = self.every.clamp(1, Self::MOST);
        self.first = self
            .first
            .rem_euclid(i64::from(self.per_bar) * i64::from(self.every));
        if !self.offset.is_finite() {
            self.offset = 0.0;
        }
        if !self.tempo.is_finite() || self.tempo < 0.0 {
            self.tempo = 0.0;
        }
        if !self.confidence.is_finite() {
            self.confidence = 0.0;
        }
        self.confidence = self.confidence.clamp(0.0, 1.0);
        self.beats.retain(|beat| beat.is_finite());
        self.beats.sort_by(f64::total_cmp);
        self.accents.resize(self.beats.len(), 0.0);
        self.accents
            .iter_mut()
            .filter(|accent| !accent.is_finite())
            .for_each(|accent| *accent = 0.0);
    }

    /// Every number in the grid, for a command's check for NaN.
    pub fn numbers(&self) -> impl Iterator<Item = f64> + '_ {
        [self.tempo, self.offset, f64::from(self.confidence)]
            .into_iter()
            .chain(self.beats.iter().copied())
            .chain(self.accents.iter().map(|accent| f64::from(*accent)))
    }
}

/// Where the moment `source` seconds into a clip's media plays on the
/// timeline, when the clip plays it: from its in-point to its end, at its
/// rate or along its speed curve. None for a moment the clip leaves out.
pub fn timeline_of(clip: &Clip, source: f64) -> Option<f64> {
    let into = source - clip.source_start;
    if !into.is_finite() || into < -1e-9 {
        return None;
    }
    let length = clip.duration;
    let consumed = speed::source_consumed(clip, length);
    if into > consumed + 1e-9 {
        return None;
    }
    let offset = match clip.speed_curve {
        None => into / clip.speed.max(1e-9),
        Some(_) => {
            // Speed is above zero along any curve, so what is consumed
            // grows with the offset: halve the interval down to a
            // hundredth of a millisecond.
            let (mut low, mut high) = (0.0f64, length);
            for _ in 0..48 {
                let middle = 0.5 * (low + high);
                if speed::source_consumed(clip, middle) < into {
                    low = middle;
                } else {
                    high = middle;
                }
                if high - low < 1e-5 {
                    break;
                }
            }
            0.5 * (low + high)
        }
    };
    Some(clip.start + offset.clamp(0.0, length))
}

/// The beats a clip plays, on the timeline, in order: the grid of its
/// media's sound, when the media has one and the clip plays its sound. A
/// picture clip whose sound was taken off to a sound clip has none; the
/// sound clip has them.
pub fn clip_beats(clip: &Clip, media: &MediaItem) -> Vec<(f64, BeatMark)> {
    let Some(grid) = media.beats.as_ref() else {
        return Vec::new();
    };
    let sounding = match clip.kind {
        ClipKind::Audio => true,
        ClipKind::Video => media.has_audio && clip.muted != Some(true),
        _ => false,
    };
    if !sounding || grid.beats.is_empty() {
        return Vec::new();
    }
    grid.marks()
        .filter_map(|(source, mark)| timeline_of(clip, source).map(|at| (at, mark)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaKind;

    fn grid(beats: usize) -> BeatGrid {
        let mut accents = vec![0.0; beats];
        // Bars start on the second beat: a pickup before the first bar.
        for accent in accents.iter_mut().skip(1).step_by(4) {
            *accent = 2.0;
        }
        BeatGrid::found(
            120.0,
            (0..beats).map(|index| 1.0 + index as f64 * 0.5).collect(),
            accents,
            0.9,
            4,
            1,
            true,
        )
    }

    fn lines(grid: &BeatGrid) -> Vec<usize> {
        (0..grid.beats.len())
            .filter(|index| grid.mark(*index) == BeatMark::Line)
            .collect()
    }

    #[test]
    fn the_grid_counts_bars_from_the_accented_beat() {
        let mut grid = grid(16);
        assert_eq!(grid.first, 1);
        assert_eq!(lines(&grid), vec![1, 5, 9, 13]);
        assert_eq!(grid.mark(2), BeatMark::Beat);

        // Every second bar.
        grid.every = 2;
        grid.tidy();
        assert_eq!(lines(&grid), vec![1, 9]);
        assert_eq!(grid.mark(5), BeatMark::Bar);
        // The other bar of each pair.
        grid.shift(4);
        assert_eq!(lines(&grid), vec![5, 13]);
        // Every beat.
        grid.bars = false;
        grid.every = 1;
        grid.tidy();
        assert_eq!(lines(&grid).len(), 16);
        // Every second beat, on the bar's first and third.
        grid.every = 2;
        grid.first = 1;
        grid.tidy();
        assert_eq!(lines(&grid), vec![1, 3, 5, 7, 9, 11, 13, 15]);
    }

    #[test]
    fn a_new_time_signature_finds_its_own_first_beat() {
        let mut grid = grid(12);
        grid.accents = vec![3.0, 0.0, 0.0, 3.0, 0.0, 0.0, 3.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        grid.count_in(3);
        assert_eq!(grid.per_bar, 3);
        assert_eq!(grid.first, 0);
        assert_eq!(lines(&grid), vec![0, 3, 6, 9]);
    }

    #[test]
    fn doubling_and_halving_the_tempo_keep_the_lines_where_they_were() {
        let grid = grid(9);
        let doubled = grid.doubled();
        assert_eq!(doubled.beats.len(), 17);
        assert_eq!(doubled.tempo, 240.0);
        assert_eq!(doubled.beats[1], 1.25);
        // The beat counted from is the same moment.
        assert_eq!(
            doubled.beats[doubled.first as usize],
            grid.beats[grid.first as usize]
        );
        let back = doubled.halved();
        assert_eq!(back.beats, grid.beats);
        assert_eq!(back.tempo, 120.0);
        assert_eq!(back.first, grid.first);
        // Halving keeps the beat the counting starts from.
        let halved = grid.halved();
        assert_eq!(halved.beats, vec![1.5, 2.5, 3.5, 4.5]);
        assert_eq!(halved.beats[halved.first as usize], 1.5);
    }

    #[test]
    fn a_grid_from_anywhere_is_tidied_into_range() {
        let mut grid = BeatGrid {
            beats: vec![2.0, f64::NAN, 1.0],
            accents: vec![1.0],
            per_bar: 0,
            every: 900,
            first: -3,
            offset: f64::INFINITY,
            confidence: 7.0,
            ..BeatGrid::default()
        };
        grid.tidy();
        assert_eq!(grid.beats, vec![1.0, 2.0]);
        assert_eq!(grid.accents.len(), 2);
        assert_eq!(grid.per_bar, 1);
        assert_eq!(grid.every, BeatGrid::MOST);
        assert_eq!(grid.first, 61);
        assert_eq!(grid.offset, 0.0);
        assert_eq!(grid.confidence, 1.0);
    }

    #[test]
    fn a_grid_round_trips_and_a_bad_one_reads_as_what_parses() {
        let grid = grid(4);
        let text = serde_json::to_string(&grid).unwrap();
        let back: BeatGrid = serde_json::from_str(&text).unwrap();
        assert_eq!(back, grid);
        let odd: BeatGrid =
            serde_json::from_str(r#"{"beats": [1.0, "x", 2.0], "accents": "no", "perBar": 3}"#)
                .unwrap();
        assert_eq!(odd.beats, vec![1.0, 2.0]);
        assert!(odd.accents.is_empty());
        assert_eq!(odd.per_bar, 3);
        assert_eq!(odd.every, 1);
    }

    fn clip(start: f64, source_start: f64, duration: f64, speed: f64) -> Clip {
        let mut clip = Clip::blank("c1", "T1", ClipKind::Audio, "song", start, duration);
        clip.media_id = "m1".to_owned();
        clip.source_start = source_start;
        clip.speed = speed;
        clip
    }

    fn media(grid: Option<BeatGrid>) -> MediaItem {
        MediaItem {
            id: "m1".to_owned(),
            kind: MediaKind::Audio,
            has_audio: true,
            beats: grid,
            ..MediaItem::default()
        }
    }

    #[test]
    fn a_clip_plays_the_beats_inside_it_where_its_in_point_and_speed_put_them() {
        let media = media(Some(grid(16)));
        // From 2.0 s into the song, at 10 s on the timeline, for 3 s.
        let at: Vec<f64> = clip_beats(&clip(10.0, 2.0, 3.0, 1.0), &media)
            .iter()
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(at, vec![10.0, 10.5, 11.0, 11.5, 12.0, 12.5, 13.0]);
        // Twice as fast: the same beats, half as far apart.
        let fast: Vec<f64> = clip_beats(&clip(10.0, 2.0, 1.5, 2.0), &media)
            .iter()
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(fast, vec![10.0, 10.25, 10.5, 10.75, 11.0, 11.25, 11.5]);
        // The offset moves every beat.
        let mut early = grid(16);
        early.offset = -0.04;
        let first = clip_beats(&clip(10.0, 2.0, 3.0, 1.0), &self::media(Some(early)))[0].0;
        assert!((first - 10.46).abs() < 1e-9);
    }

    #[test]
    fn a_curve_is_followed_and_a_silent_clip_has_no_beats() {
        let media = media(Some(grid(16)));
        let mut curved = clip(0.0, 1.0, 4.0, 1.0);
        curved.speed_curve = Some(vec![
            crate::model::SpeedPoint {
                at: 0.0,
                speed: 1.0,
                slope: None,
            },
            crate::model::SpeedPoint {
                at: 1.0,
                speed: 1.0,
                slope: None,
            },
        ]);
        let along: Vec<f64> = clip_beats(&curved, &media)
            .iter()
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(along.len(), 9);
        assert!((along[1] - 0.5).abs() < 1e-4);

        let mut picture = clip(0.0, 0.0, 4.0, 1.0);
        picture.kind = ClipKind::Video;
        picture.muted = Some(true);
        assert!(clip_beats(&picture, &media).is_empty());
        let mut title = clip(0.0, 0.0, 4.0, 1.0);
        title.kind = ClipKind::Text;
        assert!(clip_beats(&title, &media).is_empty());
        assert!(clip_beats(&clip(0.0, 0.0, 4.0, 1.0), &self::media(None)).is_empty());
    }
}
