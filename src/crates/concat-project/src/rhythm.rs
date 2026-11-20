// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: an edit's rhythm - the gaps its cuts leave on the timeline - and
//! scenes of another film laid into them, one a gap: a fan edit cut to the
//! music and the cuts of an edit someone else made to it.
//!
//! The edit's cuts are the ones found in its picture and kept on its bin
//! entry ([`crate::beats::CutMarks`]); [`rhythm`] reads its clip's gaps and
//! which of them are still empty, [`fit`] cuts a scene to its gap, and
//! [`fill`] writes the commands that lay a run of scenes in.

use crate::beats::clip_cuts;
use crate::commands::{ClipSpan, Command};
use crate::model::{Clip, ClipKind, Project, ranges};

/// The edit the timeline is cut to, and the gaps between its cuts still to
/// fill.
#[derive(Clone, Debug, PartialEq)]
pub struct Rhythm {
    /// The edit's clip.
    pub clip_id: String,
    /// The clip's name, for saying which edit it is.
    pub name: String,
    /// How many gaps it has in all, from the clip's start to its end.
    pub gaps: usize,
    /// The gaps no picture covers yet over the edit, from the playhead on,
    /// in order.
    pub free: Vec<Slot>,
}

/// An empty gap of a [`Rhythm`] and where a scene goes into it.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// Its number among the gaps, from 0.
    pub index: usize,
    /// The room in it on `lane`, in seconds on the timeline: the gap, less
    /// a hair of a clip beside it that reaches into it.
    pub start: f64,
    /// Where the room ends.
    pub end: f64,
    /// The lowest lane over the edit with nothing in the gap - a scene goes
    /// over the edit, so the gaps not yet filled still show it - or none
    /// when every lane over it has something there.
    pub lane: Option<String>,
}

/// Which part of a scene longer than its gap goes into it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Part {
    /// From its first frame.
    #[default]
    Start,
    /// As much before the gap's middle as after it.
    Middle,
    /// Up to its last frame.
    End,
}

impl Part {
    /// 0 the start, 1 the middle, 2 the end; anything else the nearest.
    pub fn from_index(index: i32) -> Part {
        match index {
            ..=0 => Part::Start,
            1 => Part::Middle,
            _ => Part::End,
        }
    }
}

/// A scene cut to its gap: where in the film it starts, how long it runs
/// on the timeline, and how fast, when not at its own pace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    /// Its in-point in the film, in seconds.
    pub source_start: f64,
    /// How long it runs on the timeline, in seconds.
    pub duration: f64,
    /// Its pace, when slowed; none at its own.
    pub speed: Option<f64>,
}

/// The rhythm of the timeline at `playhead`: of the video clips whose cuts
/// are on the lanes, the one under the playhead, else the next after it,
/// else the last before it.
///
/// A gap counts as filled while a video or a still covers it on a lane
/// over the edit's; a sound there - the music - fills nothing, and a title
/// alone leaves it empty too, though a scene then goes on a lane over the
/// title's. `tolerance` is how near two moments are one, half a frame of
/// the timeline: edges closer than that are one, a gap shorter is none,
/// and a clip reaching that little into a gap leaves it free.
pub fn rhythm(project: &Project, playhead: f64, tolerance: f64) -> Option<Rhythm> {
    let timeline = project.active();
    let playhead = playhead.max(0.0);
    let tolerance = tolerance.max(0.0);
    let templates: Vec<&Clip> = timeline
        .clips
        .iter()
        .map(|clip| clip.as_ref())
        .filter(|clip| {
            clip.kind == ClipKind::Video
                && project
                    .media_by_id(&clip.media_id)
                    .and_then(|media| media.cuts.as_ref())
                    .is_some_and(|marks| !marks.hidden)
        })
        .collect();
    let template = templates
        .iter()
        .find(|clip| clip.start <= playhead && playhead < clip.start + clip.duration)
        .or_else(|| {
            templates
                .iter()
                .filter(|clip| clip.start >= playhead)
                .min_by(|a, b| a.start.total_cmp(&b.start))
        })
        .or_else(|| templates.iter().max_by(|a, b| a.start.total_cmp(&b.start)))?;
    let media = project.media_by_id(&template.media_id)?;
    let mut edges = vec![template.start, template.start + template.duration];
    edges.extend(clip_cuts(template, media));
    edges.sort_by(f64::total_cmp);
    edges.dedup_by(|a, b| (*a - *b).abs() < tolerance);
    let gaps: Vec<(f64, f64)> = edges
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|(start, end)| end - start >= tolerance)
        .collect();
    let index = timeline
        .tracks
        .iter()
        .position(|track| track.id == template.track_id)?;
    let above: Vec<&str> = timeline.tracks[index + 1..]
        .iter()
        .map(|track| track.id.as_str())
        .collect();
    let inside = |clip: &Clip, start: f64, end: f64| {
        clip.start < end - tolerance && clip.start + clip.duration > start + tolerance
    };
    let free = gaps
        .iter()
        .enumerate()
        .filter(|(_, (_, end))| *end > playhead + tolerance)
        .filter(|(_, (start, end))| {
            !timeline.clips.iter().any(|clip| {
                matches!(clip.kind, ClipKind::Video | ClipKind::Image)
                    && above.contains(&clip.track_id.as_str())
                    && inside(clip, *start, *end)
            })
        })
        .filter_map(|(index, &(start, end))| {
            let lane = above.iter().copied().find(|lane| {
                !timeline
                    .clips
                    .iter()
                    .any(|clip| clip.track_id == *lane && inside(clip, start, end))
            });
            let (mut room_start, mut room_end) = (start, end);
            for clip in timeline
                .clips
                .iter()
                .filter(|clip| lane.is_some_and(|lane| clip.track_id == lane))
            {
                let clip_end = clip.start + clip.duration;
                if clip.start < start && clip_end > start {
                    room_start = room_start.max(clip_end);
                }
                if clip.start < end && clip_end > end {
                    room_end = room_end.min(clip.start);
                }
            }
            (room_end - room_start >= tolerance).then(|| Slot {
                index,
                start: room_start,
                end: room_end,
                lane: lane.map(str::to_owned),
            })
        })
        .collect();
    Some(Rhythm {
        clip_id: template.id.clone(),
        name: template.name.clone(),
        gaps: gaps.len(),
        free,
    })
}

/// A scene `length` seconds long from `first` into its film, cut to a gap
/// `room` seconds long. A longer scene gives the gap its start, its middle
/// or its end, as `part` says. A shorter one is slowed to fill the gap when
/// `slow` is set - never below the engine's slowest, where it stays itself
/// and leaves the rest of the gap empty rather than run on into the next
/// scene - and otherwise left short at its own pace.
pub fn fit(first: f64, length: f64, room: f64, part: Part, slow: bool) -> Fit {
    if length >= room {
        let spare = length - room;
        let offset = match part {
            Part::Start => 0.0,
            Part::Middle => spare / 2.0,
            Part::End => spare,
        };
        Fit {
            source_start: first + offset,
            duration: room,
            speed: None,
        }
    } else if slow && length > 0.0 {
        let speed = (length / room).max(ranges::MIN_SPEED);
        Fit {
            source_start: first,
            duration: (length / speed).min(room),
            speed: Some(speed),
        }
    } else {
        Fit {
            source_start: first,
            duration: length,
            speed: None,
        }
    }
}

/// The commands that lay `scenes` of `media_id` - each where it starts in
/// the film and how long it runs, in seconds - into `slots`, one a slot in
/// order, each [`fit`] to its room, at `volume` when one is given (0 for
/// none of their own sound); `fresh` is the lane for a slot that has none.
/// Scenes past the last slot are left out. Applied in one
/// [`Command::Batch`], they are one step to undo.
pub fn fill(
    media_id: &str,
    scenes: &[(f64, f64)],
    slots: &[Slot],
    part: Part,
    slow: bool,
    fresh: Option<&str>,
    volume: Option<f64>,
) -> Vec<Command> {
    scenes
        .iter()
        .zip(slots)
        .filter_map(|(&(first, length), slot)| {
            let lane = slot.lane.as_deref().or(fresh)?;
            let fit = fit(first, length, slot.end - slot.start, part, slow);
            Some(Command::AddClipSpans {
                spans: vec![ClipSpan {
                    media_id: media_id.to_owned(),
                    source_start: fit.source_start,
                    duration: fit.duration,
                    speed: fit.speed,
                    volume,
                }],
                track_id: Some(lane.to_owned()),
                start: slot.start,
                ripple: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beats::CutMarks;
    use crate::commands::NewMedia;
    use crate::editor::Editor;
    use crate::model::MediaKind;

    /// Half a frame at 30.
    const TOLERANCE: f64 = 0.5 / 30.0;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn add_media(editor: &mut Editor, path: &str, duration: f64, kind: MediaKind) -> String {
        editor
            .apply(Command::AddMedia {
                item: NewMedia {
                    path: path.to_owned(),
                    name: path.trim_start_matches('/').to_owned(),
                    duration: Some(duration),
                    kind,
                    width: (kind == MediaKind::Video).then_some(1920),
                    height: (kind == MediaKind::Video).then_some(1080),
                    frame_rate: (kind == MediaKind::Video).then_some(30.0),
                    frame_rate_fraction: (kind == MediaKind::Video).then(|| "30/1".to_owned()),
                    video_codec: (kind == MediaKind::Video).then(|| "h264".to_owned()),
                    audio_codec: Some("aac".to_owned()),
                    has_audio: true,
                    audio_tracks: Vec::new(),
                    origin: None,
                    color_space: Default::default(),
                },
            })
            .expect("adds")
            .created_id
            .expect("id")
    }

    fn add_clip(editor: &mut Editor, media_id: &str, lane: usize, start: f64) -> String {
        let track_id = editor.project().active().tracks[lane].id.clone();
        editor
            .apply(Command::AddClip {
                media_id: media_id.to_owned(),
                track_id,
                start,
                ripple: false,
            })
            .expect("adds")
            .created_id
            .expect("id")
    }

    fn lane_id(editor: &Editor, lane: usize) -> String {
        editor.project().active().tracks[lane].id.clone()
    }

    /// A ten-second edit on the bottom lane, cut at 2, 4.5 and 7 s; a film
    /// of two minutes beside it. The edit's media id, its clip's, and the
    /// film's.
    fn fixture() -> (Editor, String, String, String) {
        let mut editor = Editor::new();
        let edit = add_media(&mut editor, "/edit.mp4", 10.0, MediaKind::Video);
        editor
            .apply(Command::SetMediaCuts {
                media_id: edit.clone(),
                cuts: Some(CutMarks {
                    times: vec![2.0, 4.5, 7.0],
                    hidden: false,
                }),
            })
            .expect("sets");
        let clip = add_clip(&mut editor, &edit, 0, 0.0);
        let film = add_media(&mut editor, "/film.mkv", 120.0, MediaKind::Video);
        (editor, edit, clip, film)
    }

    fn spans(rhythm: &Rhythm) -> Vec<(usize, f64, f64)> {
        rhythm
            .free
            .iter()
            .map(|slot| (slot.index, slot.start, slot.end))
            .collect()
    }

    #[test]
    fn an_edit_with_cuts_is_a_row_of_gaps_over_it() {
        let (editor, _, clip, _) = fixture();
        let rhythm = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        assert_eq!(rhythm.clip_id, clip);
        assert_eq!(rhythm.gaps, 4);
        assert_eq!(
            spans(&rhythm),
            vec![(0, 0.0, 2.0), (1, 2.0, 4.5), (2, 4.5, 7.0), (3, 7.0, 10.0)]
        );
        let over = lane_id(&editor, 1);
        assert!(
            rhythm
                .free
                .iter()
                .all(|slot| slot.lane.as_deref() == Some(&over))
        );
    }

    #[test]
    fn no_cuts_or_hidden_ones_are_no_rhythm() {
        let (mut editor, edit, _, _) = fixture();
        editor
            .apply(Command::SetMediaCuts {
                media_id: edit.clone(),
                cuts: Some(CutMarks {
                    times: vec![2.0, 4.5, 7.0],
                    hidden: true,
                }),
            })
            .expect("sets");
        assert_eq!(rhythm(editor.project(), 0.0, TOLERANCE), None);
        editor
            .apply(Command::SetMediaCuts {
                media_id: edit,
                cuts: None,
            })
            .expect("sets");
        assert_eq!(rhythm(editor.project(), 0.0, TOLERANCE), None);
    }

    #[test]
    fn the_music_over_the_edit_fills_no_gap() {
        let (mut editor, _, _, _) = fixture();
        let music = add_media(&mut editor, "/music.mp3", 30.0, MediaKind::Audio);
        add_clip(&mut editor, &music, 1, 0.0);
        let rhythm = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        assert_eq!(rhythm.free.len(), 4, "every gap still free");
        // The scenes go on the lane over the music's.
        let over = lane_id(&editor, 2);
        assert!(
            rhythm
                .free
                .iter()
                .all(|slot| slot.lane.as_deref() == Some(&over))
        );
    }

    #[test]
    fn a_picture_over_a_gap_fills_it_and_the_playhead_starts_the_count() {
        let (mut editor, _, _, film) = fixture();
        // A scene in the second gap, on the third lane.
        let lane = lane_id(&editor, 2);
        editor
            .apply(Command::AddClipSpans {
                spans: vec![ClipSpan {
                    media_id: film.clone(),
                    source_start: 30.0,
                    duration: 2.5,
                    speed: None,
                    volume: None,
                }],
                track_id: Some(lane),
                start: 2.0,
                ripple: false,
            })
            .expect("adds");
        let found = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        assert_eq!(
            found.free.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
        // From inside the third gap: it and the fourth.
        let found = rhythm(editor.project(), 5.0, TOLERANCE).expect("a rhythm");
        assert_eq!(
            found.free.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            vec![2, 3]
        );
        // On the last frame of the second gap, the third is next.
        let found = rhythm(editor.project(), 4.5 - 1.0 / 30.0, TOLERANCE).expect("a rhythm");
        assert_eq!(found.free.first().map(|slot| slot.index), Some(2));
    }

    #[test]
    fn a_hair_of_a_clip_in_a_gap_leaves_it_free_and_shortens_its_room() {
        let (mut editor, _, _, film) = fixture();
        let lane = lane_id(&editor, 1);
        // Ends a hundredth into the second gap, less than half a frame.
        editor
            .apply(Command::AddClipSpans {
                spans: vec![ClipSpan {
                    media_id: film,
                    source_start: 10.0,
                    duration: 2.01,
                    speed: None,
                    volume: None,
                }],
                track_id: Some(lane.clone()),
                start: 0.0,
                ripple: false,
            })
            .expect("adds");
        let found = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        let second = &found.free[0];
        assert_eq!(second.index, 1);
        assert!(close(second.start, 2.01));
        assert_eq!(second.lane.as_deref(), Some(lane.as_str()));
    }

    #[test]
    fn the_playhead_picks_the_edit() {
        let (mut editor, edit, first, _) = fixture();
        let second = add_clip(&mut editor, &edit, 0, 20.0);
        let at = |playhead: f64| {
            rhythm(editor.project(), playhead, TOLERANCE)
                .expect("a rhythm")
                .clip_id
        };
        assert_eq!(at(3.0), first);
        assert_eq!(at(25.0), second);
        assert_eq!(at(15.0), second, "the next after it");
        assert_eq!(at(40.0), second, "the last before it");
    }

    #[test]
    fn scenes_fit_their_gaps() {
        // Longer: its start, middle or end.
        let start = fit(10.0, 4.0, 1.0, Part::Start, true);
        assert_eq!(
            (start.source_start, start.duration, start.speed),
            (10.0, 1.0, None)
        );
        let middle = fit(10.0, 4.0, 1.0, Part::Middle, true);
        assert_eq!(middle.source_start, 11.5);
        let end = fit(10.0, 4.0, 1.0, Part::End, true);
        assert_eq!((end.source_start, end.duration), (13.0, 1.0));
        // Shorter: slowed to fill it, or left short.
        let slowed = fit(10.0, 1.0, 4.0, Part::Start, true);
        assert_eq!(slowed.source_start, 10.0);
        assert!(close(slowed.duration, 4.0));
        assert!(close(slowed.speed.expect("slowed"), 0.25));
        let short = fit(10.0, 1.0, 4.0, Part::Start, false);
        assert_eq!((short.duration, short.speed), (1.0, None));
        // No slower than the engine goes: the scene stays itself.
        let slowest = fit(10.0, 0.1, 4.0, Part::Start, true);
        assert_eq!(slowest.speed, Some(ranges::MIN_SPEED));
        assert!(close(slowest.duration, 0.1 / ranges::MIN_SPEED));
        assert!(slowest.duration < 4.0);
    }

    #[test]
    fn a_fill_lays_scenes_over_the_edit_in_one_step() {
        let (mut editor, _, _, film) = fixture();
        let music = add_media(&mut editor, "/music.mp3", 30.0, MediaKind::Audio);
        add_clip(&mut editor, &music, 1, 0.0);
        let before = editor.project().active().clips.len();
        let found = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        // Three scenes for four gaps: a long one, a short one, an exact one.
        let scenes = [(20.0, 5.0), (40.0, 0.5), (60.0, 2.5)];
        let commands = fill(&film, &scenes, &found.free, Part::End, true, None, None);
        assert_eq!(commands.len(), 3);
        editor
            .apply_within(Some("fill"), Command::Batch { commands })
            .expect("fills");
        let timeline = editor.project().active();
        let lane = lane_id(&editor, 2);
        let mut laid: Vec<&Clip> = timeline
            .clips
            .iter()
            .map(|clip| clip.as_ref())
            .filter(|clip| clip.media_id == film)
            .collect();
        laid.sort_by(|a, b| a.start.total_cmp(&b.start));
        assert_eq!(laid.len(), 3);
        assert!(laid.iter().all(|clip| clip.track_id == lane));
        // The end of the long one into the first gap.
        assert_eq!((laid[0].start, laid[0].duration), (0.0, 2.0));
        assert_eq!(laid[0].source_start, 23.0);
        // The short one slowed to fill the second.
        assert_eq!(laid[1].start, 2.0);
        assert!(close(laid[1].duration, 2.5));
        assert!(close(laid[1].speed, 0.2));
        // The exact one into the third, at its own pace.
        assert_eq!((laid[2].start, laid[2].duration), (4.5, 2.5));
        assert_eq!(laid[2].speed, 1.0);
        // The fourth is next.
        let found = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        assert_eq!(
            found.free.iter().map(|slot| slot.index).collect::<Vec<_>>(),
            vec![3]
        );
        assert!(editor.undo());
        assert_eq!(editor.project().active().clips.len(), before, "one step");
    }

    #[test]
    fn an_edit_on_the_top_lane_needs_a_fresh_one() {
        let mut editor = Editor::new();
        let edit = add_media(&mut editor, "/edit.mp4", 10.0, MediaKind::Video);
        editor
            .apply(Command::SetMediaCuts {
                media_id: edit.clone(),
                cuts: Some(CutMarks {
                    times: vec![5.0],
                    hidden: false,
                }),
            })
            .expect("sets");
        let top = editor.project().active().tracks.len() - 1;
        add_clip(&mut editor, &edit, top, 0.0);
        let found = rhythm(editor.project(), 0.0, TOLERANCE).expect("a rhythm");
        assert_eq!(found.free.len(), 2);
        assert!(found.free.iter().all(|slot| slot.lane.is_none()));
        assert!(
            fill(
                &edit,
                &[(0.0, 1.0)],
                &found.free,
                Part::Start,
                true,
                None,
                None
            )
            .is_empty()
        );
        let fresh = editor
            .apply_within(Some("fill"), Command::AddTrack { bottom: false })
            .expect("adds")
            .created_id
            .expect("a lane");
        let commands = fill(
            &edit,
            &[(0.0, 6.0), (1.0, 6.0)],
            &found.free,
            Part::Start,
            true,
            Some(&fresh),
            Some(0.0),
        );
        editor
            .apply_within(Some("fill"), Command::Batch { commands })
            .expect("fills");
        let on_fresh: Vec<_> = editor
            .project()
            .active()
            .clips
            .iter()
            .filter(|clip| clip.track_id == fresh)
            .cloned()
            .collect();
        assert_eq!(on_fresh.len(), 2);
        assert!(
            on_fresh.iter().all(|clip| clip.volume == 0.0),
            "laid without their sound"
        );
        let lanes = editor.project().active().tracks.len();
        assert!(editor.undo());
        assert_eq!(
            editor.project().active().tracks.len(),
            lanes - 1,
            "the lane too"
        );
    }
}
