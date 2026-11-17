// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Freeze, the Tiles effect's strobe: each tile it has left behind holds
//! the moment the next one came, as a video mixer's multi-strobe did.
//!
//! A shader sees its layer's picture as it is now and nothing else, so the
//! wall cannot be drawn by the effect alone. The clip is planned as it
//! always is - the wall with every tile live - and, over it, one copy of
//! the clip for each tile already left behind: the picture at the moment
//! that tile froze, which the effect draws as that tile alone and clear
//! everywhere else (`PlannedLayer::part`). The copies ask the reader pool
//! for their pictures like any layer that is not paced, and a frozen
//! moment never moves, so after its first frame a copy's picture comes
//! from the pool's cache.
//!
//! The one place in the export and the monitor that names a package: the
//! moments are counted from the effect's knobs, which only the host can
//! turn into pictures.

use std::collections::HashMap;

use concat_core::time::{FrameRate, Rational};
use concat_core::timeline::{ClipId, Timeline};
use concat_effects::Catalogue;
use concat_project::model::AppliedFilter;
use concat_render::PlannedLayer;

use crate::resolve::quantise;

/// The Tiles package.
const TILES: &str = "concat.tiles";

/// How many tiles a wall has at most: twelve across, eight down.
const MOST: usize = 12 * 8;

/// What Freeze needs of a clip's wall at one instant: how long each tile
/// stays live, and how many tiles there are so far.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Wall {
    step: f64,
    there: usize,
}

/// The wall `effects` make `clip_time` seconds into their clip, `at` of the
/// way through it, when its Tiles freezes: building on the beat, with the
/// switch on. Counted as the shader counts, so the copies are the tiles it
/// draws.
fn wall_of(effects: &[AppliedFilter], at: f64, clip_time: f64) -> Option<Wall> {
    let applied = effects
        .iter()
        .find(|effect| effect.enabled && effect.id == TILES)?;
    let values = Catalogue::builtin()
        .get(TILES)?
        .resolve(&applied.params_at(at));
    let knob = |key: &str| values.get(key).copied().unwrap_or(0.0);
    if knob("freeze") < 0.5 {
        return None;
    }
    let per_beat = match knob("build").round() as i64 {
        1 => 1.0,
        2 => 2.0,
        3 => 4.0,
        _ => return None,
    };
    let step = 60.0 / knob("bpm").clamp(40.0, 240.0) / per_beat;
    let all = knob("across").round().clamp(1.0, 12.0) * knob("down").round().clamp(1.0, 8.0);
    let shown = (knob("shown").clamp(0.0, 100.0) / 100.0 * all - 1e-4).ceil();
    let built = (clip_time.max(0.0) / step + 1e-4).floor() + 1.0;
    let there = shown.min(built).min(all).max(0.0) as usize;
    Some(Wall { step, there })
}

/// `layers` with, after each clip whose Tiles freezes, a copy for every
/// tile it has left behind: the clip at the moment that tile froze, which
/// is when the next one came. Each moment is put on the frame grid of
/// `rate`, so a copy's picture is one the clip itself was drawn with.
pub(crate) fn with_frozen_tiles(
    layers: Vec<PlannedLayer>,
    chains: &HashMap<ClipId, Vec<AppliedFilter>>,
    timeline: &Timeline,
    time: Rational,
    rate: FrameRate,
) -> Vec<PlannedLayer> {
    let any = chains
        .values()
        .any(|effects| effects.iter().any(|effect| effect.id == TILES));
    if !any {
        return layers;
    }
    let mut out = Vec::with_capacity(layers.len());
    for mut layer in layers {
        let copies = frozen_copies(&layer, chains, timeline, time, rate);
        // The clip leaves the copies' tiles to them; see `PlannedLayer::copies`.
        layer.copies = copies.len() as u32;
        out.push(layer);
        out.extend(copies);
    }
    out
}

/// The frozen tiles' copies of `layer` at `time`, first tile first.
fn frozen_copies(
    layer: &PlannedLayer,
    chains: &HashMap<ClipId, Vec<AppliedFilter>>,
    timeline: &Timeline,
    time: Rational,
    rate: FrameRate,
) -> Vec<PlannedLayer> {
    if layer.part != 0 {
        return Vec::new();
    }
    let (Some(effects), Some(clip)) = (chains.get(&layer.clip), timeline.clip(layer.clip)) else {
        return Vec::new();
    };
    let clip_time = (time - layer.clip_start).as_f64();
    let Some(wall) = wall_of(effects, clip.fraction_at(time), clip_time) else {
        return Vec::new();
    };
    // Every tile but the newest has been left behind; the newest is the
    // clip itself, live.
    let left = wall.there.saturating_sub(1).min(MOST);
    (0..left)
        .filter_map(|place| {
            let froze = layer.clip_start.as_f64() + (place + 1) as f64 * wall.step;
            let source_time = clip.source_time_at(quantise(froze, rate))?;
            Some(PlannedLayer {
                source_time,
                paced: false,
                part: place as u32 + 1,
                copies: 0,
                ..layer.clone()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use concat_core::time::FrameRate;
    use concat_core::timeline::{Clip, MediaRef, Track, TrackKind};
    use concat_render::plan::plan_frame;

    use super::*;

    /// A wall of three by three, a tile a second (Tempo 60, every beat),
    /// on a clip from two seconds in, with the given knobs on top.
    fn wall(knobs: &[(&str, f64)]) -> (Timeline, ClipId, HashMap<ClipId, Vec<AppliedFilter>>) {
        let mut timeline = Timeline::new(640, 360, FrameRate::THIRTY);
        let track = timeline.add_track(Track::new("V1", TrackKind::Video));
        let clip = timeline
            .add_clip(
                track,
                Clip::new(
                    MediaRef::new("wall.mp4"),
                    Rational::from_int(2),
                    Rational::from_int(20),
                ),
            )
            .expect("the track exists");
        let mut tiles = AppliedFilter::new(TILES);
        for (key, value) in [
            ("across", 3.0),
            ("down", 3.0),
            ("build", 1.0),
            ("bpm", 60.0),
            ("freeze", 1.0),
        ]
        .iter()
        .chain(knobs)
        {
            tiles.params.insert((*key).to_owned(), *value);
        }
        (timeline, clip, HashMap::from([(clip, vec![tiles])]))
    }

    fn planned(
        timeline: &Timeline,
        chains: &HashMap<ClipId, Vec<AppliedFilter>>,
        seconds: f64,
    ) -> Vec<PlannedLayer> {
        let time = quantise(seconds, FrameRate::THIRTY);
        let plan = plan_frame(timeline, time);
        with_frozen_tiles(plan.layers, chains, timeline, time, FrameRate::THIRTY)
    }

    /// Three and a half seconds into the clip, four tiles have come: the
    /// fourth is the clip itself, live, and the first three are copies at
    /// the second each froze - when the next came, one, two and three
    /// seconds into the clip.
    #[test]
    fn each_tile_left_behind_is_a_copy_at_the_moment_it_froze() {
        let (timeline, clip, chains) = wall(&[]);
        let layers = planned(&timeline, &chains, 5.5);
        assert_eq!(layers.len(), 4, "the clip and three frozen tiles");
        assert_eq!(layers[0].part, 0);
        assert_eq!(layers[0].copies, 3, "the clip leaves the three to them");
        assert_eq!(layers[0].source_time, Rational::new(7, 2), "the clip, live");
        for (place, layer) in layers[1..].iter().enumerate() {
            assert_eq!(layer.clip, clip);
            assert_eq!(layer.part, place as u32 + 1);
            assert!(!layer.paced, "a frozen picture is sought, not streamed");
            assert_eq!(layer.source_time, Rational::from_int(place as i64 + 1));
        }
    }

    /// Once every tile has come, all but the last are frozen copies and the
    /// last stays live to the clip's end.
    #[test]
    fn a_full_wall_holds_all_but_its_last_tile() {
        let (timeline, _, chains) = wall(&[]);
        let layers = planned(&timeline, &chains, 15.0);
        assert_eq!(layers.len(), 9);
        assert_eq!(layers[8].source_time, Rational::from_int(8));
    }

    /// No copies without Freeze, without building on the beat, or at the
    /// wall's first tile; Shown holds the count back as the shader does.
    #[test]
    fn copies_only_where_tiles_have_frozen() {
        for knobs in [&[("freeze", 0.0)][..], &[("build", 0.0)][..]] {
            let (timeline, _, chains) = wall(knobs);
            assert_eq!(planned(&timeline, &chains, 6.0).len(), 1, "{knobs:?}");
        }
        let (timeline, _, chains) = wall(&[]);
        assert_eq!(
            planned(&timeline, &chains, 2.5).len(),
            1,
            "the first tile is live"
        );
        let (timeline, _, chains) = wall(&[("shown", 30.0)]);
        // Three of nine shown: the third live, two frozen.
        assert_eq!(planned(&timeline, &chains, 10.0).len(), 3);
    }

    /// A clip with no Tiles is planned as it was.
    #[test]
    fn a_clip_without_tiles_is_untouched() {
        let (timeline, clip, _) = wall(&[]);
        let chains = HashMap::from([(clip, vec![AppliedFilter::new("concat.mono")])]);
        assert_eq!(planned(&timeline, &chains, 6.0).len(), 1);
    }
}
