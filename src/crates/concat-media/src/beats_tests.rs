// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The beat finder against music made here, where every hit is known: a
//! kick, a snare, hi-hats, a bass line and a pad that change chord with the
//! bar, and a voice over the chorus - the GIF Visualizer's test song
//! (`test/make_audio.py` there), with its silences, its drumless break and
//! its quiet verse eighteen decibels down, at any tempo.

use super::*;

/// A little white noise without a random-number crate: xorshift, then the
/// sum of four uniforms, near enough to a bell for a drum's rattle.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        let mut sum = 0.0;
        for _ in 0..4 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            sum += (self.0 >> 11) as f64 / (1u64 << 53) as f64;
        }
        (sum - 2.0) * 1.7
    }
}

struct Synth {
    out: Vec<f64>,
    noise: Noise,
}

impl Synth {
    fn new(seconds: f64, seed: u64) -> Synth {
        Synth {
            out: vec![0.0; (seconds * f64::from(RATE)) as usize],
            noise: Noise(seed.max(1)),
        }
    }

    fn add(&mut self, at: f64, length: f64, mut sample: impl FnMut(f64, &mut Noise) -> f64) {
        let start = (at * f64::from(RATE)).round() as usize;
        let count = (length * f64::from(RATE)) as usize;
        for index in 0..count {
            let Some(slot) = self.out.get_mut(start + index) else {
                break;
            };
            *slot += sample(index as f64 / f64::from(RATE), &mut self.noise);
        }
    }

    fn kick(&mut self, at: f64, amp: f64) {
        let mut phase = 0.0;
        self.add(at, 0.28, |t, noise| {
            phase +=
                2.0 * std::f64::consts::PI * (48.0 + 110.0 * (-t / 0.03).exp()) / f64::from(RATE);
            amp * (phase.sin() * (-t / 0.09).exp() + 0.25 * noise.next() * (-t / 0.004).exp())
        });
    }

    fn snare(&mut self, at: f64, amp: f64) {
        self.add(at, 0.18, |t, noise| {
            amp * (0.6 * (2.0 * std::f64::consts::PI * 190.0 * t).sin() * (-t / 0.05).exp()
                + 0.7 * noise.next() * (-t / 0.045).exp())
        });
    }

    fn hat(&mut self, at: f64, amp: f64) {
        let mut last = 0.0;
        self.add(at, 0.05, |t, noise| {
            let now = noise.next() * (-t / 0.012).exp();
            let high = now - last;
            last = now;
            amp * high
        });
    }

    fn tone(&mut self, from: f64, to: f64, hz: f64, amp: f64, harmonics: &[f64]) {
        let length = to - from;
        self.add(from, length, |t, _| {
            let envelope = (t / 0.02).min(1.0) * ((length - t) / 0.03).clamp(0.0, 1.0);
            let sound: f64 = harmonics
                .iter()
                .enumerate()
                .map(|(k, weight)| {
                    weight * (2.0 * std::f64::consts::PI * hz * (k + 1) as f64 * t).sin()
                })
                .sum();
            amp * envelope * sound
        });
    }

    /// Something like singing: a gliding, wavering voice in syllables.
    fn voice(&mut self, from: f64, to: f64, amp: f64) {
        let mut phase = 0.0;
        self.add(from, to - from, |t, noise| {
            let pitch = 260.0
                + 60.0 * (2.0 * std::f64::consts::PI * 0.23 * t).sin()
                + 4.0 * (2.0 * std::f64::consts::PI * 5.5 * t).sin();
            phase += 2.0 * std::f64::consts::PI * pitch / f64::from(RATE);
            let syllable = 0.55
                + 0.45
                    * (2.0 * std::f64::consts::PI * 3.1 * t + 1.0).sin()
                    * (2.0 * std::f64::consts::PI * 0.7 * t).sin();
            amp * syllable
                * (phase.sin()
                    + 0.5 * (2.0 * phase).sin()
                    + 0.3 * (3.0 * phase).sin()
                    + 0.05 * noise.next())
        });
    }

    /// A mastered track: gently limited, peaking a little under full scale.
    fn finish(self) -> Vec<f32> {
        let limited: Vec<f64> = self
            .out
            .iter()
            .map(|x| (x * 1.6).tanh() / 1.6f64.tanh())
            .collect();
        let peak = limited
            .iter()
            .fold(0.0f64, |peak, x| peak.max(x.abs()))
            .max(1e-9);
        limited.iter().map(|x| (x * 0.89 / peak) as f32).collect()
    }
}

/// A song made here, and where its music really is.
struct Song {
    samples: Vec<f32>,
    /// Every kick, which is every beat the drums play.
    kicks: Vec<f64>,
    /// Where each bar starts, with the drums or without.
    bars: Vec<f64>,
    /// The beats the drums do not play, in the break: where the grid should
    /// carry on.
    carried: Vec<f64>,
}

/// The chords a bar at a time, A minor, F, C, G: the bass's root and the
/// pad's three notes.
const CHORDS: [(f64, [f64; 3]); 4] = [
    (55.0, [220.0, 261.63, 329.63]),
    (43.65, [174.61, 220.0, 261.63]),
    (65.41, [261.63, 329.63, 392.0]),
    (49.0, [196.0, 246.94, 293.66]),
];

/// How the drums go.
#[derive(Clone, Copy, PartialEq)]
enum Groove {
    /// A kick on every beat, the snare on two and four, hats on the eighths.
    Floor,
    /// The kick on one and three only, the snare on two and four.
    Rock,
}

/// The GIF Visualizer's song at `bpm`: two bars of silence, eight of verse,
/// eight of louder chorus with a voice, two of break with no drums, two of
/// silence, eight of a verse 18 dB down, and two of silence.
fn song(bpm: f64, groove: Groove, seed: u64) -> Song {
    let beat = 60.0 / bpm;
    let bar = 4.0 * beat;
    let at = |bars: f64| bars * bar;
    let mut synth = Synth::new(at(32.0), seed);
    let mut song = Song {
        samples: Vec::new(),
        kicks: Vec::new(),
        bars: Vec::new(),
        carried: Vec::new(),
    };
    let drums = |synth: &mut Synth, song: &mut Song, from: f64, to: f64, amp: f64| {
        let beats = ((to - from) / beat).round() as usize;
        for index in 0..beats {
            let time = from + index as f64 * beat;
            let in_bar = index % 4;
            if groove == Groove::Floor || in_bar % 2 == 0 {
                synth.kick(time, amp);
                song.kicks.push(time);
            }
            if in_bar % 2 == 1 {
                synth.snare(time, amp * 0.6);
                if groove == Groove::Rock {
                    song.kicks.push(time);
                }
            }
            synth.hat(time + beat / 2.0, amp * 0.25);
            synth.hat(time, amp * 0.15);
        }
    };
    let harmony = |synth: &mut Synth, song: &mut Song, from: f64, to: f64, bass: f64, pad: f64| {
        let bars = ((to - from) / bar).round() as usize;
        for index in 0..bars {
            let start = from + index as f64 * bar;
            song.bars.push(start);
            let (root, notes) = CHORDS[index % 4];
            synth.tone(start, start + bar, root, bass, &[1.0, 0.5, 0.25]);
            for note in notes {
                synth.tone(start, start + bar, note, pad, &[1.0, 0.3]);
            }
        }
    };
    // Verse.
    drums(&mut synth, &mut song, at(2.0), at(10.0), 0.55);
    harmony(&mut synth, &mut song, at(2.0), at(10.0), 0.16, 0.03);
    // Chorus.
    drums(&mut synth, &mut song, at(10.0), at(18.0), 0.6);
    harmony(&mut synth, &mut song, at(10.0), at(18.0), 0.22, 0.06);
    synth.voice(at(10.0), at(18.0), 0.22);
    // The break: no drums, the grid has to carry on through it.
    harmony(&mut synth, &mut song, at(18.0), at(20.0), 0.0, 0.09);
    synth.voice(at(18.0), at(20.0), 0.10);
    song.carried = (0..8).map(|index| at(18.0) + index as f64 * beat).collect();
    // The quiet verse.
    let quiet = 0.125;
    drums(&mut synth, &mut song, at(22.0), at(30.0), 0.55 * quiet);
    harmony(
        &mut synth,
        &mut song,
        at(22.0),
        at(30.0),
        0.16 * quiet,
        0.03 * quiet,
    );
    song.samples = synth.finish();
    song
}

fn analyse(samples: &[f32]) -> Analysis {
    let mut detector = Detector::new();
    // In pieces of odd sizes, as a decoder hands them over.
    for piece in samples.chunks(1153) {
        detector.push(piece);
    }
    detector.finish()
}

/// The detected beat nearest `time`, and how far from it.
fn nearest(beats: &[f64], time: f64) -> (usize, f64) {
    beats
        .iter()
        .enumerate()
        .map(|(index, beat)| (index, beat - time))
        .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .unwrap_or((0, f64::INFINITY))
}

/// The share of `times` with a detected beat within `tolerance` seconds.
fn hit_rate(beats: &[f64], times: &[f64], tolerance: f64) -> f64 {
    let hits = times
        .iter()
        .filter(|time| nearest(beats, **time).1.abs() <= tolerance)
        .count();
    hits as f64 / times.len().max(1) as f64
}

fn check_song(bpm: f64, groove: Groove) {
    let song = song(bpm, groove, 7);
    let found = analyse(&song.samples);
    assert!(
        (found.tempo - bpm).abs() < 1.0,
        "{bpm} BPM was heard as {:.2}",
        found.tempo
    );
    let rate = hit_rate(&found.beats, &song.kicks, 0.02);
    assert!(
        rate >= 0.95,
        "{bpm} BPM: only {:.0} % of the kicks have a beat within 20 ms",
        rate * 100.0
    );
    // Through the break the grid carries on where the beats would be.
    let carried = hit_rate(&found.beats, &song.carried, 0.04);
    assert!(
        carried >= 0.85,
        "{bpm} BPM: the grid fell apart in the break ({:.0} %)",
        carried * 100.0
    );
    // Nothing in the silence before the music or after it.
    let first = song.kicks[0];
    let last = *song.kicks.last().unwrap();
    assert!(
        found
            .beats
            .iter()
            .all(|beat| *beat > first - 0.1 && *beat < last + 0.1),
        "{bpm} BPM: beats in the silence: first {:?}, last {:?}",
        found.beats.first(),
        found.beats.last()
    );
    // No beat twice, none skipped where the drums play.
    for pair in found.beats.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap > 0.6 * 60.0 / bpm,
            "{bpm} BPM: two beats {gap:.3} s apart at {:.2}",
            pair[0]
        );
    }
    assert!(
        found.confidence > 0.6,
        "{bpm} BPM: confidence {}",
        found.confidence
    );
}

#[test]
fn a_four_on_the_floor_song_is_counted_at_its_tempo_on_its_kicks() {
    check_song(120.0, Groove::Floor);
}

#[test]
fn slower_and_faster_songs_are_counted_at_their_own_tempo() {
    for bpm in [86.0, 100.0, 128.0, 140.0] {
        check_song(bpm, Groove::Floor);
    }
}

#[test]
fn a_rock_beat_with_the_kick_on_one_and_three_is_counted_in_quarters() {
    for bpm in [95.0, 123.0] {
        check_song(bpm, Groove::Rock);
    }
}

#[test]
fn the_beat_is_placed_on_the_hit_not_late_or_early() {
    let song = song(120.0, Groove::Floor, 11);
    let found = analyse(&song.samples);
    let errors: Vec<f64> = song
        .kicks
        .iter()
        .map(|kick| nearest(&found.beats, *kick).1)
        .filter(|error| error.abs() < 0.03)
        .collect();
    let mean = errors.iter().sum::<f64>() / errors.len() as f64;
    let spread = (errors
        .iter()
        .map(|error| (error - mean).powi(2))
        .sum::<f64>()
        / errors.len() as f64)
        .sqrt();
    eprintln!(
        "beat - kick: mean {:.2} ms, spread {:.2} ms",
        mean * 1000.0,
        spread * 1000.0
    );
    assert!(
        mean.abs() < 0.004,
        "beats sit {:.1} ms off the kicks on average",
        mean * 1000.0
    );
    assert!(
        spread < 0.006,
        "beats wander {:.1} ms around the kicks",
        spread * 1000.0
    );
}

#[test]
fn the_bar_starts_where_the_chord_changes_with_the_kick() {
    for (bpm, groove) in [
        (120.0, Groove::Floor),
        (100.0, Groove::Rock),
        (133.0, Groove::Floor),
    ] {
        let song = song(bpm, groove, 3);
        let found = analyse(&song.samples);
        let phase = downbeat(&found.accents, 4);
        let starts: Vec<usize> = song
            .bars
            .iter()
            .filter(|bar| song.kicks.iter().any(|kick| (kick - **bar).abs() < 0.001))
            .map(|bar| nearest(&found.beats, *bar).0)
            .collect();
        let right = starts
            .iter()
            .filter(|index| (**index + 4 - phase).is_multiple_of(4))
            .count();
        assert!(
            right as f64 >= 0.9 * starts.len() as f64,
            "{bpm} BPM: the bar was counted from beat {phase}; {right} of {} bar starts fall on it",
            starts.len()
        );
    }
}

#[test]
fn a_pickup_before_the_first_bar_is_counted_as_the_last_beat_of_a_bar() {
    let bpm = 120.0;
    let mut song = song(bpm, Groove::Floor, 5);
    // A snare one beat before the first bar: the drummer's count-in.
    let mut synth = Synth::new(song.samples.len() as f64 / f64::from(RATE), 9);
    synth.out = song.samples.iter().map(|x| f64::from(*x)).collect();
    let pickup = song.bars[0] - 60.0 / bpm;
    synth.snare(pickup, 0.4);
    synth.kick(pickup, 0.3);
    song.samples = synth.finish();
    let found = analyse(&song.samples);
    let (first_bar, error) = nearest(&found.beats, song.bars[0]);
    assert!(error.abs() < 0.02);
    let phase = downbeat(&found.accents, 4);
    assert_eq!(phase, first_bar % 4, "the pickup was taken for a downbeat");
}

#[test]
fn a_band_that_speeds_up_is_followed() {
    // From 116 to 124 BPM over the song, beat by beat.
    let mut synth = Synth::new(70.0, 21);
    let mut kicks = Vec::new();
    let mut time = 2.0;
    let mut index = 0;
    while time < 66.0 {
        let bpm = 116.0 + 8.0 * (time - 2.0) / 64.0;
        synth.kick(time, 0.55);
        kicks.push(time);
        if index % 2 == 1 {
            synth.snare(time, 0.33);
        }
        synth.hat(time + 30.0 / bpm, 0.14);
        time += 60.0 / bpm;
        index += 1;
    }
    let found = analyse(&synth.finish());
    let rate = hit_rate(&found.beats, &kicks, 0.025);
    assert!(
        rate >= 0.9,
        "only {:.0} % of a drifting band's kicks were followed",
        rate * 100.0
    );
    assert!(
        (found.tempo - 120.0).abs() < 3.0,
        "tempo {:.1}",
        found.tempo
    );
}

#[test]
fn silence_noise_and_a_moment_have_no_beats_and_do_not_fail() {
    let silence = analyse(&vec![0.0; 10 * RATE as usize]);
    assert!(silence.beats.is_empty());
    assert_eq!(silence.tempo, 0.0);

    let moment = analyse(&vec![0.1; RATE as usize / 2]);
    assert!(moment.beats.is_empty());

    let mut noise = Noise(99);
    let hiss: Vec<f32> = (0..8 * RATE as usize)
        .map(|_| (0.1 * noise.next()) as f32)
        .collect();
    let found = analyse(&hiss);
    assert_eq!(found.beats.len(), found.accents.len());
    assert!(found.beats.windows(2).all(|pair| pair[1] > pair[0]));
}

#[test]
fn the_phase_is_the_most_accented_and_any_bar_length_works() {
    let accents = [0.1, 2.0, -0.5, 0.0, 0.2, 1.8, -0.4, 0.1, 0.0, 2.2];
    assert_eq!(downbeat(&accents, 4), 1);
    assert_eq!(downbeat(&accents, 1), 0);
    assert_eq!(downbeat(&[], 4), 0);
    // Three to a bar: phase 1 holds 2.0, 0.2 and 0.1, more on average than
    // phase 0's 0.1, 0.0, -0.4 and 2.2 or phase 2's -0.5, 1.8 and 0.0.
    assert_eq!(downbeat(&accents, 3), 1);
    // More beats to a bar than there are beats: only the beats there are.
    assert_eq!(downbeat(&accents[..2], 4), 1);
}

#[test]
fn the_fft_finds_a_sine_in_its_bin() {
    let size = 64;
    let fft = Fft::new(size);
    let mut re: Vec<f32> = (0..size)
        .map(|index| (2.0 * std::f64::consts::PI * 5.0 * index as f64 / size as f64).cos() as f32)
        .collect();
    let mut im = vec![0.0; size];
    fft.run(&mut re, &mut im);
    let magnitude: Vec<f32> = re
        .iter()
        .zip(&im)
        .map(|(r, i)| (r * r + i * i).sqrt())
        .collect();
    assert!((magnitude[5] - 32.0).abs() < 1e-3);
    assert!((magnitude[59] - 32.0).abs() < 1e-3);
    assert!(
        magnitude
            .iter()
            .enumerate()
            .all(|(bin, value)| bin == 5 || bin == 59 || *value < 1e-3)
    );
}
