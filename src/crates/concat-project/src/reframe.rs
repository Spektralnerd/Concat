// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: the pictures that covered a timeline's frame, carried to a new
//! frame so they cover that one too.
//!
//! A picture's scale counts from its fit in the frame, so a fitted picture
//! stays fitted whatever the frame becomes. A fill is different: it is a
//! number worked out for one frame (see [`crate::model::Framing::Fill`]),
//! and a wide shot filled in a wide frame is a band across the middle of a
//! tall one. [`refill`] works out the edits that keep every picture that
//! covered the old frame covering the new one - its zoom over the fill
//! kept, and each offset kept as the same share of the room the picture
//! has to move in, so a shot framed on its top edge stays framed on it.

use crate::commands::Command;
use crate::model::{Clip, KeyProperty, Project, ranges};

/// How far a scale may fall short of the fill and still count as one: a
/// fill is written rounded to a millionth.
const FILL_SLACK: f64 = 1e-4;

/// How far an offset may run past the room it has and still count as
/// inside it, for the same rounding.
const EDGE_SLACK: f64 = 1e-4;

/// A turn within this many degrees of a quarter is a quarter: the room a
/// picture has to move in is a rectangle only on the square.
const SQUARE_SLACK_DEGREES: f64 = 0.5;

/// Two numbers this close are the same number, so a frame that only grew
/// (1080p to 4K) moves nothing.
const SAME: f64 = 1e-9;

/// What [`refill`] found to do.
#[derive(Debug, Default)]
pub struct Refill {
    /// The edits, in clip order. Applied as one `Command::Batch` they are one
    /// undo step, apart from the frame change they follow.
    pub commands: Vec<Command>,
    /// How many pictures they touch.
    pub clips: usize,
}

/// The edits that carry the active timeline's pictures from a `from` frame
/// to a `to` frame, both in pixels.
///
/// A picture counts as covering when its scale is at least the fill - its
/// one scale, or every key of a keyed one. Each scale is multiplied by the
/// new fill over the old, and each offset becomes the same share of the
/// new room as it was of the old. An offset already past the edge of its
/// room - a picture slid partway out on purpose - is left as it was, and so
/// is every picture that did not cover the old frame: a fitted one is
/// still fitted, a picture-in-picture still its share of the frame. A
/// picture with a keyed turn is left alone too, its fill changing from
/// moment to moment.
pub fn refill(project: &Project, from: (u32, u32), to: (u32, u32)) -> Refill {
    let mut found = Refill::default();
    if from == to || from.0 == 0 || from.1 == 0 || to.0 == 0 || to.1 == 0 {
        return found;
    }
    for clip in &project.active().clips {
        let Some((width, height)) = project
            .media_by_id(&clip.media_id)
            .and_then(|item| Some((item.width?, item.height?)))
        else {
            continue;
        };
        let commands = refill_clip(clip, (width, height), from, to);
        if !commands.is_empty() {
            found.clips += 1;
            found.commands.extend(commands);
        }
    }
    found
}

/// [`refill`] for one clip, from media `width` by `height`.
fn refill_clip(clip: &Clip, media: (u32, u32), from: (u32, u32), to: (u32, u32)) -> Vec<Command> {
    if !clip.kind.is_visual()
        || media.0 == 0
        || media.1 == 0
        || clip.is_keyed(KeyProperty::Rotation)
    {
        return Vec::new();
    }
    let crop = clip.crop.unwrap_or_default().tidy();
    let shape = Shape {
        picture: (
            f64::from(media.0) * (1.0 - crop.left - crop.right),
            f64::from(media.1) * (1.0 - crop.top - crop.bottom),
        ),
        rotation: clip.rotation,
        stretch: (clip.stretch_x, clip.stretch_y),
    };
    let (Some(old_fill), Some(new_fill)) = (shape.fill(from), shape.fill(to)) else {
        return Vec::new();
    };
    let scales: Vec<f64> = if clip.is_keyed(KeyProperty::Scale) {
        clip.keys_on(KeyProperty::Scale)
            .map(|key| key.value)
            .collect()
    } else {
        vec![clip.scale]
    };
    if scales
        .iter()
        .any(|scale| !scale.is_finite() || *scale < old_fill * (1.0 - FILL_SLACK))
    {
        return Vec::new();
    }
    let ratio = new_fill / old_fill;
    let rescale = |scale: f64| {
        if (ratio - 1.0).abs() < SAME {
            scale
        } else {
            (scale * ratio).clamp(ranges::MIN_SCALE, ranges::MAX_SCALE)
        }
    };
    // The scale an offset is measured against: where it is keyed, the
    // scale at that key; otherwise the smallest the picture gets, which has
    // the least room, so the carried offset covers at every other scale too.
    let smallest = scales.iter().copied().fold(f64::INFINITY, f64::min);
    let carry = |property: KeyProperty, offset: f64, at: Option<f64>| {
        let scale = at.map_or(smallest, |at| clip.value_at(KeyProperty::Scale, at));
        let (Some(old_room), Some(new_room)) =
            (shape.room(from, scale), shape.room(to, rescale(scale)))
        else {
            return offset;
        };
        let (old_room, new_room) = match property {
            KeyProperty::OffsetX => (old_room.0, new_room.0),
            _ => (old_room.1, new_room.1),
        };
        carried(offset, old_room, new_room)
    };

    let changed = |before: f64, after: f64| (after - before).abs() > SAME;
    let mut commands = Vec::new();
    let scale = Some(rescale(clip.scale))
        .filter(|after| !clip.is_keyed(KeyProperty::Scale) && changed(clip.scale, *after));
    let offset_x = Some(carry(KeyProperty::OffsetX, clip.offset_x, None))
        .filter(|after| !clip.is_keyed(KeyProperty::OffsetX) && changed(clip.offset_x, *after));
    let offset_y = Some(carry(KeyProperty::OffsetY, clip.offset_y, None))
        .filter(|after| !clip.is_keyed(KeyProperty::OffsetY) && changed(clip.offset_y, *after));
    if scale.is_some() || offset_x.is_some() || offset_y.is_some() {
        commands.push(Command::SetClipTransform {
            clip_id: clip.id.clone(),
            scale,
            offset_x,
            offset_y,
            rotation: None,
            stretch_x: None,
            stretch_y: None,
        });
    }
    for key in &clip.keys {
        let value = match key.property {
            KeyProperty::Scale => rescale(key.value),
            KeyProperty::OffsetX | KeyProperty::OffsetY => {
                carry(key.property, key.value, Some(key.at))
            }
            _ => continue,
        };
        if changed(key.value, value) {
            commands.push(Command::SetClipKey {
                clip_id: clip.id.clone(),
                property: key.property,
                at: key.at,
                value,
                ease: key.ease,
            });
        }
    }
    commands
}

/// An offset carried from the room it had to the room it has: the same
/// share of it, or as it was when it stood past the edge.
fn carried(offset: f64, old_room: f64, new_room: f64) -> f64 {
    let old_room = old_room.max(0.0);
    let new_room = new_room.max(0.0);
    if offset.abs() > old_room + EDGE_SLACK || (new_room - old_room).abs() < SAME {
        return offset;
    }
    if old_room <= EDGE_SLACK {
        return 0.0;
    }
    // Plus zero: a share of no room is 0, not the -0 a negative offset
    // times nothing would write.
    (offset / old_room * new_room).clamp(-new_room, new_room) + 0.0
}

/// A picture as a frame sees it: its pixels after the crop, its turn and
/// its stretch.
struct Shape {
    picture: (f64, f64),
    rotation: f64,
    stretch: (f64, f64),
}

impl Shape {
    /// The picture fitted in a `width` by `height` frame: its size at scale
    /// one, before the stretch.
    fn fitted(&self, width: f64, height: f64) -> (f64, f64) {
        let (cw, ch) = self.picture;
        let fit = (width / cw).min(height / ch);
        (cw * fit, ch * fit)
    }

    /// The scale at which the picture, turned and stretched as it is, covers
    /// `frame` centred: the frame turned into the picture's own axes, inside
    /// the picture. [`crate::model::framing_scale`]'s fill, with the stretch
    /// in it.
    fn fill(&self, frame: (u32, u32)) -> Option<f64> {
        let (width, height) = (f64::from(frame.0), f64::from(frame.1));
        let (cw, ch) = self.picture;
        let (sx, sy) = self.stretch;
        if !(cw > 0.0
            && ch > 0.0
            && sx > 0.0
            && sy > 0.0
            && sx.is_finite()
            && sy.is_finite()
            && self.rotation.is_finite())
        {
            return None;
        }
        let (fw, fh) = self.fitted(width, height);
        let turn = self.rotation.to_radians();
        let (cos, sin) = (turn.cos().abs(), turn.sin().abs());
        let fill = ((width * cos + height * sin) / (fw * sx))
            .max((width * sin + height * cos) / (fh * sy));
        (fill.is_finite() && fill > 0.0).then_some(fill)
    }

    /// How far the picture at `scale` may move each way and still cover
    /// `frame`, as fractions of the frame's width and height - the offsets'
    /// own unit. None for a picture turned off the square, whose room is no
    /// rectangle.
    fn room(&self, frame: (u32, u32), scale: f64) -> Option<(f64, f64)> {
        let quarters = self.rotation / 90.0;
        if (quarters - quarters.round()).abs() * 90.0 > SQUARE_SLACK_DEGREES {
            return None;
        }
        let (width, height) = (f64::from(frame.0), f64::from(frame.1));
        let (fw, fh) = self.fitted(width, height);
        let across = fw * self.stretch.0 * scale;
        let down = fh * self.stretch.1 * scale;
        // A quarter turn stands the picture on its side.
        let (across, down) = if (quarters.round() as i64).rem_euclid(2) == 1 {
            (down, across)
        } else {
            (across, down)
        };
        Some(((across / width - 1.0) / 2.0, (down / height - 1.0) / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::NewMedia;
    use crate::editor::Editor;
    use crate::model::{Framing, KeyEase, MediaKind, VideoSettings};

    /// An editor with one clip of a `width` by `height` video, in a `frame`.
    fn editor_with(width: u32, height: u32, frame: (u32, u32)) -> (Editor, String) {
        let mut editor = Editor::new();
        let media_id = editor
            .apply(Command::AddMedia {
                item: NewMedia {
                    path: "/scene.mp4".to_owned(),
                    name: "scene.mp4".to_owned(),
                    duration: Some(10.0),
                    kind: MediaKind::Video,
                    width: Some(width),
                    height: Some(height),
                    frame_rate: Some(24.0),
                    frame_rate_fraction: Some("24/1".to_owned()),
                    video_codec: Some("h264".to_owned()),
                    audio_codec: None,
                    has_audio: false,
                    audio_tracks: Vec::new(),
                    origin: None,
                    color_space: Default::default(),
                },
            })
            .expect("adds")
            .created_id
            .expect("id");
        let track_id = editor.project().active().tracks[0].id.clone();
        let clip_id = editor
            .apply(Command::AddClip {
                media_id,
                track_id,
                start: 0.0,
                ripple: false,
            })
            .expect("adds")
            .created_id
            .expect("id");
        set_frame(&mut editor, frame);
        (editor, clip_id)
    }

    fn set_frame(editor: &mut Editor, (width, height): (u32, u32)) {
        let timeline_id = editor.project().active_timeline_id.clone();
        editor
            .apply(Command::SetTimelineVideo {
                timeline_id,
                video: VideoSettings {
                    width,
                    height,
                    ..VideoSettings::default()
                },
            })
            .expect("sets the frame");
    }

    fn transform(editor: &mut Editor, clip_id: &str, scale: f64, offset: (f64, f64)) {
        editor
            .apply(Command::SetClipTransform {
                clip_id: clip_id.to_owned(),
                scale: Some(scale),
                offset_x: Some(offset.0),
                offset_y: Some(offset.1),
                rotation: None,
                stretch_x: None,
                stretch_y: None,
            })
            .expect("places");
    }

    /// The frame changed the way the window changes it: the frame first,
    /// then the refill as a step of its own. The clip count comes back.
    fn reframe(editor: &mut Editor, to: (u32, u32)) -> usize {
        let video = editor.project().active().video;
        let from = (video.width, video.height);
        set_frame(editor, to);
        let found = refill(editor.project(), from, to);
        if !found.commands.is_empty() {
            editor
                .apply(Command::Batch {
                    commands: found.commands,
                })
                .expect("refills");
        }
        found.clips
    }

    fn clip(editor: &Editor) -> &Clip {
        &editor.project().active().clips[0]
    }

    /// What Fill gives the clip in the frame it is in now.
    fn fill_now(editor: &Editor) -> f64 {
        let project = editor.project();
        let clip = &project.active().clips[0];
        let item = project.media_by_id(&clip.media_id).expect("media");
        let video = project.active().video;
        clip.framing_scale(
            Framing::Fill,
            (item.width, item.height),
            (video.width, video.height),
            0.0,
        )
        .expect("a fill")
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// A wide shot that covered a wide frame covers a tall one: the scale
    /// Fill would give it there.
    #[test]
    fn a_wide_shot_that_filled_a_wide_frame_fills_a_tall_one() {
        let (mut editor, _) = editor_with(1920, 1080, (1920, 1080));
        assert_eq!(reframe(&mut editor, (1080, 1920)), 1);
        // 1920x1080 fitted into 1080x1920 is 1080x607.5; covering 1920 rows
        // takes 1920 / 607.5.
        assert!(
            close(clip(&editor).scale, 1920.0 / 607.5),
            "{}",
            clip(&editor).scale
        );
        assert!(close(clip(&editor).scale, fill_now(&editor)));
    }

    /// The edit's own scenes: 1480x1080 filled into 1920x1080 and framed on
    /// their top edge. In a tall frame they fill again, centred, since a
    /// tall frame leaves no room up and down; in a wider frame the top edge
    /// stays the top edge.
    #[test]
    fn a_shot_framed_on_its_top_edge_keeps_its_framing_where_there_is_room() {
        let (mut editor, clip_id) = editor_with(1480, 1080, (1920, 1080));
        let fill = fill_now(&editor);
        assert!(close(fill, 1920.0 / 1480.0));
        // The room up and down at the fill: half the overhang, as a share
        // of the frame's height.
        let room = (1080.0 * fill / 1080.0 - 1.0) / 2.0;
        transform(&mut editor, &clip_id, fill, (0.0, room));

        assert_eq!(reframe(&mut editor, (2560, 1080)), 1);
        let wide_fill = fill_now(&editor);
        let wide_room = (1080.0 * wide_fill / 1080.0 - 1.0) / 2.0;
        assert!(close(clip(&editor).scale, wide_fill));
        assert!(
            close(clip(&editor).offset_y, wide_room),
            "{}",
            clip(&editor).offset_y
        );
        assert_eq!(clip(&editor).offset_x, 0.0);

        assert_eq!(reframe(&mut editor, (1080, 1920)), 1);
        assert!(close(clip(&editor).scale, fill_now(&editor)));
        assert_eq!((clip(&editor).offset_x, clip(&editor).offset_y), (0.0, 0.0));
    }

    /// A push past the fill stays a push past it: 1.2 times the fill before
    /// is 1.2 times the fill after, and an offset half way across its room
    /// stays half way.
    #[test]
    fn the_zoom_over_the_fill_and_the_share_of_the_room_are_kept() {
        let (mut editor, clip_id) = editor_with(1920, 1080, (1920, 1080));
        // At 1.2 the picture overhangs a tenth of the frame each side.
        transform(&mut editor, &clip_id, 1.2, (0.05, -0.05));
        reframe(&mut editor, (1080, 1080));
        let fill = fill_now(&editor);
        assert!(close(clip(&editor).scale, 1.2 * fill));
        // The square's room across at 1.2 times its fill, and up and down.
        let across = (1080.0 * 1.2 * fill / 1080.0 - 1.0) / 2.0;
        let down = (607.5 * 1.2 * fill / 1080.0 - 1.0) / 2.0;
        assert!(
            close(clip(&editor).offset_x, across / 2.0),
            "{}",
            clip(&editor).offset_x
        );
        assert!(
            close(clip(&editor).offset_y, -down / 2.0),
            "{}",
            clip(&editor).offset_y
        );
    }

    /// Bars stay bars: a fitted shot of another shape did not cover the old
    /// frame, and is not made to cover the new one. Neither is a picture
    /// in a corner.
    #[test]
    fn a_fitted_shot_and_a_picture_in_picture_are_left_alone() {
        let (mut editor, clip_id) = editor_with(1440, 1080, (1920, 1080));
        assert_eq!(reframe(&mut editor, (1080, 1920)), 0);
        assert_eq!(clip(&editor).scale, 1.0);

        transform(&mut editor, &clip_id, 0.4, (0.3, 0.3));
        assert_eq!(reframe(&mut editor, (1920, 1080)), 0);
        assert_eq!((clip(&editor).scale, clip(&editor).offset_x), (0.4, 0.3));
    }

    /// A shot slid out past its edge - the start of a slide - is somewhere
    /// on purpose, and stays there.
    #[test]
    fn an_offset_past_the_edge_is_left_where_it_was() {
        let (mut editor, clip_id) = editor_with(1920, 1080, (1920, 1080));
        transform(&mut editor, &clip_id, 1.0, (-1.0, 0.0));
        assert_eq!(reframe(&mut editor, (1080, 1920)), 1);
        assert!(close(clip(&editor).scale, fill_now(&editor)));
        assert_eq!(clip(&editor).offset_x, -1.0);
    }

    /// A keyed push-in rides on: every key of the scale is carried by the
    /// same factor, and the keys keep their places and eases.
    #[test]
    fn keys_are_carried_and_keep_their_places_and_eases() {
        let (mut editor, clip_id) = editor_with(1920, 1080, (1920, 1080));
        for (at, value) in [(0.0, 1.0), (1.0, 1.5)] {
            editor
                .apply(Command::SetClipKey {
                    clip_id: clip_id.clone(),
                    property: KeyProperty::Scale,
                    at,
                    value,
                    ease: KeyEase::LINEAR,
                })
                .expect("keys");
        }
        let eases: Vec<KeyEase> = clip(&editor)
            .keys_on(KeyProperty::Scale)
            .map(|key| key.ease)
            .collect();
        reframe(&mut editor, (1080, 1920));
        let fill = fill_now(&editor);
        let keys: Vec<(f64, f64, KeyEase)> = clip(&editor)
            .keys_on(KeyProperty::Scale)
            .map(|key| (key.at, key.value, key.ease))
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!((keys[0].0, keys[1].0), (0.0, 1.0));
        assert!(
            close(keys[0].1, fill) && close(keys[1].1, 1.5 * fill),
            "{keys:?}"
        );
        assert_eq!(vec![keys[0].2, keys[1].2], eases);
        assert_eq!(
            clip(&editor).scale,
            1.0,
            "the constant under the keys is untouched"
        );
    }

    /// A frame that only grows keeps its shape, and nothing moves.
    #[test]
    fn a_frame_of_the_same_shape_moves_nothing() {
        let (mut editor, clip_id) = editor_with(1480, 1080, (1920, 1080));
        transform(&mut editor, &clip_id, 1.297297, (0.0, 0.1486));
        assert_eq!(reframe(&mut editor, (3840, 2160)), 0);
        assert_eq!(
            (clip(&editor).scale, clip(&editor).offset_y),
            (1.297297, 0.1486)
        );
    }

    /// A shot on its side fills by its other edge: a wide shot turned a
    /// quarter is a tall picture, and a wider frame asks more of it.
    #[test]
    fn a_quarter_turn_fills_by_the_other_edge() {
        let (mut editor, clip_id) = editor_with(1920, 1080, (1920, 1080));
        editor
            .apply(Command::SetClipTransform {
                clip_id: clip_id.clone(),
                scale: None,
                offset_x: None,
                offset_y: None,
                rotation: Some(90.0),
                stretch_x: None,
                stretch_y: None,
            })
            .expect("turns");
        let project = editor.project();
        let item = project.media_by_id(&clip(&editor).media_id).expect("media");
        let fill = clip(&editor)
            .framing_scale(Framing::Fill, (item.width, item.height), (1920, 1080), 0.0)
            .expect("a fill");
        transform(&mut editor, &clip_id, fill, (0.0, 0.0));
        // Every frame narrower than the shot fits it by its width, and the
        // turned picture covers each at the same scale: nothing to do.
        assert_eq!(reframe(&mut editor, (1080, 1920)), 0);
        assert_eq!(reframe(&mut editor, (1920, 1080)), 0);
        assert_eq!(reframe(&mut editor, (2560, 1080)), 1);
        let project = editor.project();
        let item = project.media_by_id(&clip(&editor).media_id).expect("media");
        let after = clip(&editor)
            .framing_scale(Framing::Fill, (item.width, item.height), (2560, 1080), 0.0)
            .expect("a fill");
        assert!(
            close(clip(&editor).scale, after),
            "{} {after}",
            clip(&editor).scale
        );
    }

    /// The refill is a step of its own: one undo leaves the new frame with
    /// the pictures as they were, the next takes the frame back.
    #[test]
    fn one_undo_leaves_the_pictures_as_they_were() {
        let (mut editor, _) = editor_with(1920, 1080, (1920, 1080));
        reframe(&mut editor, (1080, 1920));
        editor.undo();
        assert_eq!(clip(&editor).scale, 1.0);
        assert_eq!(editor.project().active().video.width, 1080);
        editor.undo();
        assert_eq!(editor.project().active().video.width, 1920);
    }
}
