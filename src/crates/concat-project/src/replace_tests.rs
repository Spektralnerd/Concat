// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: [`Command::ReplaceClipSource`] - another scene in a clip's place,
//! and nothing else moving.

use crate::commands::{ClipSpan, Command, NewMedia};
use crate::editor::Editor;
use crate::model::{Clip, ClipKind, MediaKind, ranges};

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

fn add_span(
    editor: &mut Editor,
    media_id: &str,
    start: f64,
    source_start: f64,
    duration: f64,
) -> String {
    let track_id = editor.project().active().tracks[0].id.clone();
    editor
        .apply(Command::AddClipSpans {
            spans: vec![ClipSpan {
                media_id: media_id.to_owned(),
                source_start,
                duration,
                speed: None,
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

fn replace(
    editor: &mut Editor,
    clip_id: &str,
    media_id: &str,
    source_start: f64,
    duration: f64,
    speed: f64,
) -> bool {
    editor
        .apply(Command::ReplaceClipSource {
            clip_id: clip_id.to_owned(),
            media_id: media_id.to_owned(),
            source_start,
            duration,
            speed,
        })
        .expect("applies")
        .applied
}

/// Three scenes cut end to end - 0-2, 2-4.5, 4.5-7.5 s - and two more
/// scenes in the bin, a long one and a short one.
fn cut() -> (Editor, [String; 3], String, String) {
    let mut editor = Editor::new();
    let a = add_media(&mut editor, "/scene-1.mp4", 6.0, MediaKind::Video);
    let b = add_media(&mut editor, "/scene-2.mp4", 6.0, MediaKind::Video);
    let c = add_media(&mut editor, "/scene-3.mp4", 6.0, MediaKind::Video);
    let clips = [
        add_span(&mut editor, &a, 0.0, 1.0, 2.0),
        add_span(&mut editor, &b, 2.0, 0.5, 2.5),
        add_span(&mut editor, &c, 4.5, 0.0, 3.0),
    ];
    let long = add_media(&mut editor, "/scene-1184.mp4", 8.0, MediaKind::Video);
    let short = add_media(&mut editor, "/scene-1160.mp4", 1.0, MediaKind::Video);
    (editor, clips, long, short)
}

#[test]
fn a_scene_takes_the_place_and_nothing_else_moves() {
    let (mut editor, [a, b, c], long, _) = cut();
    // Looks on the middle clip that should stay.
    editor
        .apply(Command::UpdateClip {
            clip_id: b.clone(),
            patch: serde_json::from_value(serde_json::json!({ "opacity": 0.5 })).expect("a patch"),
        })
        .expect("updates");
    let before = [clip(&editor, &a), clip(&editor, &b), clip(&editor, &c)];
    assert!(replace(&mut editor, &b, &long, 5.5, 2.5, 1.0));
    let after = clip(&editor, &b);
    assert_eq!(after.media_id, long);
    assert_eq!(after.name, "scene-1184.mp4");
    assert_eq!(
        (after.start, after.duration, after.source_start),
        (2.0, 2.5, 5.5)
    );
    assert_eq!(after.track_id, before[1].track_id);
    assert_eq!(after.opacity, 0.5, "its looks stay");
    assert_eq!(clip(&editor, &a), before[0], "the clip before is untouched");
    assert_eq!(clip(&editor, &c), before[2], "the clip after is untouched");
    // One step to undo.
    assert!(editor.undo());
    assert_eq!(clip(&editor, &b), before[1]);
}

#[test]
fn a_replacement_never_reaches_into_the_next_clip_nor_past_its_file() {
    let (mut editor, [_, b, c], long, short) = cut();
    let after_c = clip(&editor, &c);
    // Asked for longer: held to the room up to the next clip.
    assert!(replace(&mut editor, &b, &long, 0.0, 9.0, 1.0));
    assert_eq!(clip(&editor, &b).duration, 2.5);
    // Late in its file: held to what is left of it.
    assert!(replace(&mut editor, &b, &long, 7.0, 2.5, 1.0));
    assert!((clip(&editor, &b).duration - 1.0).abs() < 1e-9);
    // A one-second scene at its own pace: one second, the rest a gap.
    assert!(replace(&mut editor, &b, &short, 0.0, 2.5, 1.0));
    let shortened = clip(&editor, &b);
    assert!((shortened.duration - 1.0).abs() < 1e-9);
    // Slowed to fill it instead: it grows back into the gap it left, and
    // no further.
    assert!(replace(&mut editor, &b, &short, 0.0, 2.5, 0.4));
    let slowed = clip(&editor, &b);
    assert_eq!(slowed.speed, 0.4);
    assert!((slowed.duration - 2.5).abs() < 1e-9);
    assert_eq!(clip(&editor, &c), after_c, "nothing after it moved");
}

#[test]
fn copies_of_the_old_file_go_with_it() {
    let (mut editor, [_, b, _], long, _) = cut();
    editor
        .apply(Command::SetClipSpeedCurve {
            clip_id: b.clone(),
            curve: Some(vec![
                crate::model::SpeedPoint {
                    at: 0.0,
                    speed: 0.5,
                    slope: None,
                },
                crate::model::SpeedPoint {
                    at: 1.0,
                    speed: 1.5,
                    slope: None,
                },
            ]),
            ripple: false,
        })
        .expect("curves");
    let length = clip(&editor, &b).duration;
    assert!(replace(&mut editor, &b, &long, 1.0, length, 1.0));
    let after = clip(&editor, &b);
    assert_eq!(after.speed_curve, None);
    assert_eq!(after.smooth, None);
    assert_eq!(after.enhanced, None);
}

#[test]
fn a_still_or_the_wrong_kind() {
    let (mut editor, [_, b, _], _, _) = cut();
    let still = add_media(&mut editor, "/poster.png", 0.0, MediaKind::Image);
    assert!(replace(&mut editor, &b, &still, 3.0, 2.5, 0.5));
    let shown = clip(&editor, &b);
    assert_eq!(shown.kind, ClipKind::Image);
    assert_eq!(
        (shown.source_start, shown.speed, shown.duration),
        (0.0, 1.0, 2.5)
    );
    // A song is no picture; nothing happens.
    let song = add_media(&mut editor, "/song.mp3", 200.0, MediaKind::Audio);
    let before = clip(&editor, &b);
    assert!(!replace(&mut editor, &b, &song, 0.0, 2.5, 1.0));
    assert_eq!(clip(&editor, &b), before);
    assert!(!replace(&mut editor, "c999", &song, 0.0, 2.5, 1.0));
    assert!(!replace(&mut editor, &b, "m999", 0.0, 2.5, 1.0));
    // Held to the speeds a clip can have.
    let (mut editor, [_, b, _], long, _) = cut();
    assert!(replace(&mut editor, &b, &long, 0.0, 2.5, 0.0001));
    assert_eq!(clip(&editor, &b).speed, ranges::MIN_SPEED);
}
