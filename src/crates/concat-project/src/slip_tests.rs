// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: [`Command::SlipClip`] - another stretch of a clip's footage in the
//! same place, and nothing else moving.

use crate::commands::{ClipSpan, Command, CommandError, NewMedia};
use crate::editor::Editor;
use crate::model::{Clip, MediaKind, SpeedPoint};
use crate::speed;

fn add_media(editor: &mut Editor, path: &str, duration: f64, kind: MediaKind) -> String {
    let video = kind == MediaKind::Video;
    editor
        .apply(Command::AddMedia {
            item: NewMedia {
                path: path.to_owned(),
                name: path.trim_start_matches('/').to_owned(),
                duration: (kind != MediaKind::Image).then_some(duration),
                kind,
                width: (kind != MediaKind::Audio).then_some(1920),
                height: (kind != MediaKind::Audio).then_some(1080),
                frame_rate: video.then_some(24.0),
                frame_rate_fraction: video.then(|| "24/1".to_owned()),
                video_codec: video.then(|| "h264".to_owned()),
                audio_codec: (kind != MediaKind::Image).then(|| "aac".to_owned()),
                has_audio: kind != MediaKind::Image,
                audio_tracks: Vec::new(),
                origin: None,
                color_space: Default::default(),
            },
        })
        .expect("adds")
        .created_id
        .expect("id")
}

/// A clip of `media_id` on the bottom lane at `start`, showing `duration`
/// seconds from `source_start` at `speed`.
fn add_span(
    editor: &mut Editor,
    media_id: &str,
    start: f64,
    source_start: f64,
    duration: f64,
    speed: Option<f64>,
) -> String {
    let track_id = editor.project().active().tracks[0].id.clone();
    editor
        .apply(Command::AddClipSpans {
            spans: vec![ClipSpan {
                media_id: media_id.to_owned(),
                source_start,
                duration,
                speed,
                volume: None,
            }],
            track_id: Some(track_id),
            start,
            ripple: false,
        })
        .expect("adds")
        .created_id
        .expect("id")
}

fn clip(editor: &Editor, id: &str) -> Clip {
    editor.project().active().clip(id).expect("exists").clone()
}

fn slip(editor: &mut Editor, id: &str, source_start: f64) -> bool {
    editor
        .apply(Command::SlipClip {
            clip_id: id.to_owned(),
            source_start,
        })
        .expect("slips")
        .applied
}

/// Three shots of a two-minute film cut end to end: 0-2, 2-4.5, 4.5-7.5 s.
fn three_shots() -> (Editor, String, [String; 3]) {
    let mut editor = Editor::new();
    let film = add_media(&mut editor, "/film.mkv", 120.0, MediaKind::Video);
    let a = add_span(&mut editor, &film, 0.0, 10.0, 2.0, None);
    let b = add_span(&mut editor, &film, 2.0, 30.0, 2.5, None);
    let c = add_span(&mut editor, &film, 4.5, 50.0, 3.0, None);
    (editor, film, [a, b, c])
}

#[test]
fn a_slip_moves_the_in_point_and_nothing_else() {
    let (mut editor, _, [a, b, c]) = three_shots();
    let before = [clip(&editor, &a), clip(&editor, &b), clip(&editor, &c)];
    assert!(slip(&mut editor, &b, 41.25));
    let mut expected = before[1].clone();
    expected.source_start = 41.25;
    assert_eq!(clip(&editor, &b), expected, "only the in-point changed");
    assert_eq!(clip(&editor, &a), before[0], "the shot before is untouched");
    assert_eq!(clip(&editor, &c), before[2], "the shot after is untouched");
    // Still end to end: no gap, no overlap.
    let b = clip(&editor, &b);
    assert_eq!(b.start, 2.0);
    assert_eq!(b.start + b.duration, 4.5);
}

#[test]
fn a_slip_holds_to_the_footage_there_is() {
    let (mut editor, _, [a, b, _]) = three_shots();
    assert!(slip(&mut editor, &b, 500.0));
    assert_eq!(
        clip(&editor, &b).source_start,
        117.5,
        "its last frame is the film's"
    );
    assert!(slip(&mut editor, &b, -3.0));
    assert_eq!(
        clip(&editor, &b).source_start,
        0.0,
        "its first frame is the film's"
    );
    // Already at the start: nothing to do, nothing recorded.
    assert!(!slip(&mut editor, &b, -1.0));
    // The first shot of the timeline slips like any other.
    assert!(slip(&mut editor, &a, 3.0));
    assert_eq!(clip(&editor, &a).start, 0.0);
    assert_eq!(clip(&editor, &a).source_start, 3.0);
}

#[test]
fn a_slowed_shot_slips_through_what_it_plays() {
    let mut editor = Editor::new();
    let film = add_media(&mut editor, "/film.mkv", 120.0, MediaKind::Video);
    // Four seconds on the timeline, two of the film.
    let slow = add_span(&mut editor, &film, 0.0, 10.0, 4.0, Some(0.5));
    assert!(slip(&mut editor, &slow, 1000.0));
    let slowed = clip(&editor, &slow);
    assert_eq!(slowed.source_start, 118.0);
    assert_eq!((slowed.duration, slowed.speed), (4.0, 0.5));

    // Along a curve: what the curve plays of it fits in the film.
    let curved = add_span(&mut editor, &film, 10.0, 20.0, 4.0, None);
    editor
        .apply(Command::SetClipSpeedCurve {
            clip_id: curved.clone(),
            curve: Some(vec![
                SpeedPoint {
                    at: 0.0,
                    speed: 0.5,
                    slope: None,
                },
                SpeedPoint {
                    at: 1.0,
                    speed: 2.0,
                    slope: None,
                },
            ]),
            ripple: false,
        })
        .expect("curves");
    let before = clip(&editor, &curved);
    assert!(slip(&mut editor, &curved, 1000.0));
    let after = clip(&editor, &curved);
    let span = speed::source_consumed(&after, after.duration);
    assert!((after.source_start + span - 120.0).abs() < 1e-9);
    assert_eq!(
        (after.start, after.duration, &after.speed_curve),
        (before.start, before.duration, &before.speed_curve)
    );
}

#[test]
fn a_still_has_nothing_to_slip() {
    let mut editor = Editor::new();
    let picture = add_media(&mut editor, "/still.png", 0.0, MediaKind::Image);
    let still = add_span(&mut editor, &picture, 0.0, 0.0, 3.0, None);
    let before = clip(&editor, &still);
    assert!(!slip(&mut editor, &still, 5.0));
    assert_eq!(clip(&editor, &still), before);
    assert!(
        !slip(&mut editor, "c999", 5.0),
        "an unknown clip is no slip"
    );
}

#[test]
fn a_drag_of_slips_is_one_step_to_undo() {
    let (mut editor, _, [_, b, _]) = three_shots();
    for at in [31.0, 32.5, 29.0, 35.0] {
        editor
            .apply_within(
                Some("slip drag"),
                Command::SlipClip {
                    clip_id: b.clone(),
                    source_start: at,
                },
            )
            .expect("slips");
    }
    assert_eq!(clip(&editor, &b).source_start, 35.0);
    assert!(editor.undo());
    assert_eq!(
        clip(&editor, &b).source_start,
        30.0,
        "back where the drag began"
    );
    assert!(editor.redo());
    assert_eq!(clip(&editor, &b).source_start, 35.0);
}

#[test]
fn a_slip_to_no_number_is_refused() {
    let (mut editor, _, [_, b, _]) = three_shots();
    let refused = editor.apply(Command::SlipClip {
        clip_id: b.clone(),
        source_start: f64::NAN,
    });
    assert!(matches!(refused, Err(CommandError::NotANumber)));
    assert_eq!(clip(&editor, &b).source_start, 30.0);
}
