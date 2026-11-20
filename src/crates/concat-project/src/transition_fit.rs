// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: where a transition across a cut goes, so that both sides move.
//!
//! A transition over a cut draws both pictures for its whole span: the
//! outgoing clip on past its out-point, the incoming one early from before
//! its in-point. Where a side has no footage there, the export holds that
//! side's frame instead - and a picture that fades in frozen and jumps into
//! motion at the cut reads as a fault. Centred on the cut stays the rule
//! while both sides have the footage; a side short of it moves the span
//! towards the other side, as far as that side's footage goes, and where
//! neither has enough the span sits where the least is held.

use crate::model::{Clip, ClipKind};

/// The footage `clip` has before its in-point, in timeline seconds: none
/// under a speed curve, whose clock the export does not run backwards, and
/// all that is wanted of anything that is not a video - a picture, a title
/// or a shape is the same picture earlier.
pub fn head_room(clip: &Clip) -> f64 {
    if clip.kind != ClipKind::Video {
        return f64::INFINITY;
    }
    if clip
        .speed_curve
        .as_ref()
        .is_some_and(|curve| !curve.is_empty())
    {
        return 0.0;
    }
    (clip.source_start / clip.speed.max(f64::EPSILON)).max(0.0)
}

/// The footage `clip` has after its out-point, in timeline seconds, a
/// `frame` short of the file's `media_duration` as the export counts it, so
/// a container that rounds its length up is not read past its last
/// picture. None where the length is unknown or a speed curve sets the
/// clock; all that is wanted of anything that is not a video.
pub fn tail_room(clip: &Clip, media_duration: Option<f64>, frame: f64) -> f64 {
    if clip.kind != ClipKind::Video {
        return f64::INFINITY;
    }
    if clip
        .speed_curve
        .as_ref()
        .is_some_and(|curve| !curve.is_empty())
    {
        return 0.0;
    }
    media_duration.map_or(0.0, |length| {
        let speed = clip.speed.max(f64::EPSILON);
        ((length - clip.source_start) / speed - clip.duration - frame).max(0.0)
    })
}

/// Where a transition `duration` long across the cut at `cut` starts, given
/// `head` seconds of the incoming clip's footage before the cut and `tail`
/// seconds of the outgoing clip's after it.
///
/// Of the span, `before` lies ahead of the cut and the rest after it. Both
/// sides move throughout when `before <= head` and `duration - before <=
/// tail`; of the spans that do, the one nearest centred. Where none does,
/// the time held is `before - head` ahead of the cut plus `duration -
/// before - tail` after it - the same anywhere between `head` and
/// `duration - tail` - and again the one nearest centred.
pub fn transition_start(cut: f64, duration: f64, head: f64, tail: f64) -> f64 {
    let duration = duration.max(0.0);
    let head = head.max(0.0).min(duration);
    let tail = tail.max(0.0).min(duration);
    let earliest = duration - tail;
    let (from, to) = if earliest <= head {
        (earliest, head)
    } else {
        (head, earliest)
    };
    cut - (duration / 2.0).clamp(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Footage enough on both sides: centred, as ever.
    #[test]
    fn with_footage_on_both_sides_it_is_centred() {
        assert!(close(transition_start(10.0, 1.0, 5.0, 5.0), 9.5));
        assert!(close(
            transition_start(10.0, 1.0, f64::INFINITY, f64::INFINITY),
            9.5
        ));
    }

    /// An incoming clip from its file's first frame: the span starts at the
    /// cut, on the outgoing clip's footage, and nothing is held.
    #[test]
    fn an_incoming_clip_without_footage_before_it_starts_the_span_at_the_cut() {
        assert!(close(transition_start(10.0, 1.0, 0.0, 5.0), 10.0));
        // Some footage: as much of the span ahead of the cut as it covers.
        assert!(close(transition_start(10.0, 1.0, 0.2, 5.0), 9.8));
    }

    /// An outgoing clip to its file's last frame: the span ends at the cut.
    #[test]
    fn an_outgoing_clip_without_footage_after_it_ends_the_span_at_the_cut() {
        assert!(close(transition_start(10.0, 1.0, 5.0, 0.0), 9.0));
        assert!(close(transition_start(10.0, 1.0, 5.0, 0.3), 9.3));
    }

    /// The ending of the BTTF edit: the knob from its first frame, the flux
    /// capacitor with 0.573 s after its out-point, and a 0.6 s dissolve.
    /// Neither side covers it all; the span sits where under a frame is
    /// held, nearest the middle.
    #[test]
    fn where_neither_side_has_enough_the_least_is_held() {
        let start = transition_start(29.4, 0.6, 0.0, 0.573);
        assert!(close(start, 29.4 - 0.027), "{start}");
        // Both short alike: centred, the hold shared.
        assert!(close(transition_start(10.0, 1.0, 0.1, 0.1), 9.5));
    }

    /// A video's room is its footage in timeline seconds; a speed curve
    /// has none, and a picture has all it wants.
    #[test]
    fn rooms_count_footage_at_the_clips_speed() {
        let mut clip = Clip {
            kind: ClipKind::Video,
            source_start: 2.0,
            duration: 3.0,
            speed: 2.0,
            ..Clip::default()
        };
        assert!(close(head_room(&clip), 1.0));
        // 20 s of file from 2 s at twice the speed: 9 s, 3 of them used.
        assert!(close(tail_room(&clip, Some(20.0), 0.04), 6.0 - 0.04));
        assert_eq!(tail_room(&clip, None, 0.04), 0.0);
        clip.speed_curve = Some(vec![crate::model::SpeedPoint {
            at: 0.0,
            speed: 1.0,
            slope: None,
        }]);
        assert_eq!(
            (head_room(&clip), tail_room(&clip, Some(20.0), 0.04)),
            (0.0, 0.0)
        );
        clip.kind = ClipKind::Image;
        assert_eq!(head_room(&clip), f64::INFINITY);
    }
}
