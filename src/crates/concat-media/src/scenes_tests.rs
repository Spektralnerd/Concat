// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use super::*;

#[test]
fn frames_are_analysed_with_the_longer_side_at_256() {
    assert_eq!(analysis_size(1920, 1080), (256, 144));
    // The BTTF fullscreen hybrid: 1080 / 5.78125 is 186.8.
    assert_eq!(analysis_size(1480, 1080), (256, 187));
    assert_eq!(analysis_size(720, 576), (256, 205));
    assert_eq!(analysis_size(1080, 1920), (144, 256));
    assert_eq!(analysis_size(512, 288), (256, 144));
    // No shrinking at 256 or less.
    assert_eq!(analysis_size(256, 144), (256, 144));
    assert_eq!(analysis_size(200, 100), (200, 100));
}

#[test]
fn the_shortest_scene_is_rounded_as_python_rounds() {
    let settings = Settings::default();
    assert_eq!(settings.min_frames(24000.0 / 1001.0), 14);
    assert_eq!(settings.min_frames(24.0), 14);
    assert_eq!(settings.min_frames(25.0), 15);
    assert_eq!(settings.min_frames(30000.0 / 1001.0), 18);
    assert_eq!(settings.min_frames(30.0), 18);
    assert_eq!(settings.min_frames(60.0), 36);
    let none = Settings {
        min_seconds: 0.0,
        ..Settings::default()
    };
    assert_eq!(none.min_frames(25.0), 0);
}

#[test]
fn hsv_is_opencvs() {
    // cv2.cvtColor(..., cv2.COLOR_BGR2HSV) on these BGR triples.
    let cases = [
        ((0, 0, 0), (0, 0, 0)),
        ((255, 255, 255), (0, 0, 255)),
        ((0, 0, 255), (0, 255, 255)),
        ((0, 255, 0), (60, 255, 255)),
        ((255, 0, 0), (120, 255, 255)),
        ((10, 200, 30), (57, 242, 200)),
        ((200, 30, 10), (117, 242, 200)),
        ((30, 10, 200), (177, 242, 200)),
        ((128, 128, 127), (90, 2, 128)),
        ((17, 99, 203), (13, 234, 203)),
        ((250, 1, 250), (150, 254, 250)),
        ((1, 2, 3), (15, 170, 3)),
        ((77, 77, 200), (0, 157, 200)),
    ];
    let hsv = Hsv::new();
    for ((b, g, r), want) in cases {
        assert_eq!(hsv.of(b, g, r), want, "BGR {b} {g} {r}");
    }
}

#[test]
fn matrices_are_swscales() {
    // The x86 coefficients swscale makes for BT.601 and BT.709, limited.
    let bt601 = Matrix::of(2, false);
    assert_eq!(
        (
            bt601.y_offset,
            bt601.y,
            bt601.ub,
            bt601.ug,
            bt601.vg,
            bt601.vr
        ),
        (128, 9539, 16525, -3209, -6660, 13075)
    );
    let bt709 = Matrix::of(1, false);
    assert_eq!(
        (bt709.y, bt709.ub, bt709.ug, bt709.vg, bt709.vr),
        (9539, 17305, -1747, -4366, 14686)
    );
    assert_eq!(bt601.bgr(16, 128, 128), [0, 0, 0]);
    assert_eq!(bt601.bgr(235, 128, 128), [255, 255, 255]);
    let full = Matrix::of(2, true);
    assert_eq!(full.bgr(0, 128, 128), [0, 0, 0]);
    assert_eq!(full.bgr(255, 128, 128), [255, 255, 255]);
    // A space swscale has no table for is BT.601.
    assert_eq!(Matrix::of(8, false), bt601);
    assert_eq!(Matrix::of(40, false), bt601);
}

#[test]
fn taps_weigh_to_2048_and_stay_inside() {
    for (source, target) in [
        (1920, 256),
        (1080, 144),
        (1480, 256),
        (1080, 187),
        (300, 256),
    ] {
        for clamp in [true, false] {
            let taps = taps(source, target, clamp);
            assert_eq!(taps.len(), target);
            for tap in &taps {
                assert!(tap.near < source && tap.far < source);
                assert!((2047..=2049).contains(&(tap.near_weight + tap.far_weight)));
            }
        }
    }
    // 1920 to 256: the first output pixel sits 3.25 pixels in.
    let first = taps(1920, 256, true)[0];
    assert_eq!((first.near, first.far), (3, 4));
    assert_eq!((first.near_weight, first.far_weight), (1536, 512));
}

/// Cuts a run of frames over the threshold (`above`) makes through a
/// filter of `min_len` frames.
fn cuts_of(min_len: u64, merge: bool, frames: u64, above: &[u64]) -> Vec<u64> {
    let mut filter = Filter::new(min_len, merge);
    (0..frames)
        .filter_map(|frame| filter.feed(frame, above.contains(&frame)))
        .collect()
}

#[test]
fn a_flash_is_merged_into_the_cut_before_it() {
    // Frame 6 comes a frame after the cut at 5: merged, and the merge only
    // ends with the next cut, which it reports once a shortest scene has
    // passed after it.
    assert_eq!(cuts_of(3, true, 30, &[5, 6, 20]), vec![5, 20]);
    // Suppressed, frame 6 is simply dropped.
    assert_eq!(cuts_of(3, false, 30, &[5, 6, 20]), vec![5, 20]);
}

#[test]
fn quick_cuts_merge_at_their_last() {
    // 10 cuts; 12 to 15 come too soon after it and after each other, so
    // they merge, and once the merge spans a shortest scene it reports its
    // last frame, 15, three frames under the threshold later.
    assert_eq!(cuts_of(3, true, 40, &[10, 12, 13, 14, 15]), vec![10, 15]);
    // Suppressed, every cut a shortest scene after the last one stands.
    assert_eq!(cuts_of(3, false, 40, &[10, 12, 13, 14, 15]), vec![10, 13]);
    // A merge shorter than a shortest scene never ends on its own.
    assert_eq!(cuts_of(3, true, 40, &[10, 12, 13, 14]), vec![10]);
}

#[test]
fn no_shortest_scene_lets_every_frame_cut() {
    assert_eq!(cuts_of(0, true, 10, &[1, 2, 3]), vec![1, 2, 3]);
}

#[test]
fn the_first_cut_waits_for_a_shortest_scene() {
    assert_eq!(cuts_of(14, true, 100, &[3, 40]), vec![40]);
}

#[test]
fn pictures_are_taken_where_scenedetect_takes_them() {
    assert_eq!(picture_frames(0, 100), [1, 50, 99]);
    // 17.5 rounds to the even 18.
    assert_eq!(picture_frames(10, 25), [11, 18, 24]);
    assert_eq!(picture_frames(5, 6), [5, 5, 5]);
    assert_eq!(picture_frames(5, 7), [6, 6, 6]);
    assert_eq!(picture_frames(7, 10), [8, 8, 9]);
}

/// A grey frame whose level says which frame it is.
fn numbered(index: u64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, 64, 32);
    let level = 16 + (index as u8 % 50) * 4;
    for plane in 0..3 {
        let fill = if plane == 0 { level } else { 128 };
        frame.data_mut(plane).fill(fill);
    }
    frame
}

/// The frame a grey picture shows, read back from its level.
fn number_of(rgb: &[u8]) -> u64 {
    // Limited range stretched to full: level 16 + 4n shows as about
    // (4n) * 255 / 219.
    let shown = f64::from(rgb[rgb.len() / 2]);
    (shown * 219.0 / 255.0 / 4.0).round() as u64
}

#[test]
fn each_scene_gets_its_three_pictures_from_the_right_frames() {
    let painter = Painter::new((64, 32), 0, 32);
    let mut keeper = Keeper::new(painter);
    let mut filter = Filter::new(3, true);
    // Cuts at 20 and 45 straight away, then a quick run from 46 to 49
    // that merges into one at 49, known only at 52.
    let above = [20u64, 45, 46, 47, 48, 49];
    let frames = 90u64;
    let mut got: Vec<(usize, u8, u64, u64)> = Vec::new();
    let mut cuts = Vec::new();
    for index in 0..frames {
        let frame = numbered(index);
        let is_above = above.contains(&index);
        let cut = filter.feed(index, is_above).filter(|&cut| cut > 0);
        if let Some(cut) = cut {
            cuts.push(cut);
        }
        keeper.frame(index, &frame, is_above, cut, &mut |event| {
            if let Event::Picture {
                scene,
                index,
                frame,
                rgb,
                ..
            } = event
            {
                got.push((scene, index, frame, number_of(rgb)));
            }
        });
    }
    keeper.finish(frames, &mut |event| {
        if let Event::Picture {
            scene,
            index,
            frame,
            rgb,
            ..
        } = event
        {
            got.push((scene, index, frame, number_of(rgb)));
        }
    });
    assert_eq!(cuts, vec![20, 45, 49]);
    let scenes = [(0u64, 20u64), (20, 45), (45, 49), (49, 90)];
    for (number, &(start, end)) in scenes.iter().enumerate() {
        let wanted = picture_frames(start, end);
        for picture in 1..=3u8 {
            let (_, _, frame, shown) = *got
                .iter()
                .find(|(scene, index, _, _)| *scene == number + 1 && *index == picture)
                .unwrap_or_else(|| panic!("scene {} picture {picture} missing", number + 1));
            // The painted picture is the frame it says it is.
            assert_eq!(shown, frame % 50, "scene {} picture {picture}", number + 1);
            assert!(
                frame >= start && frame < end,
                "scene {} picture {picture}",
                number + 1
            );
            if picture != 2 {
                assert_eq!(
                    frame,
                    wanted[usize::from(picture) - 1],
                    "scene {} picture {picture}",
                    number + 1
                );
            } else {
                // The middle is near it: within an eighth of the scene.
                let off = frame.abs_diff(wanted[1]);
                assert!(
                    off * 8 <= end - start + 8,
                    "scene {} middle {frame} wanted {}",
                    number + 1,
                    wanted[1]
                );
            }
        }
    }
    assert_eq!(got.len(), 12);
}
