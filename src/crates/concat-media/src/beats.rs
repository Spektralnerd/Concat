// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Beats: where the pulse of a piece of music falls, found in its sound.
//!
//! The sound is decoded to mono at [`RATE`] and cut into short, overlapping
//! frames. Each frame's spectrum is summed into third-octave bands, and how
//! much each band grew since a moment before is the frame's *onset*: a drum
//! hit, a strum or a new note makes the bands jump, a held note does not.
//! The low bands, where the kick drum is, are counted twice - the same
//! measurement the GIF Visualizer moves its picture by, "a drum hit gives a
//! short pulse; a steady bass line gives nothing" - because the kick is what
//! most music is counted along.
//!
//! From that onset curve the tempo is the spacing it repeats at most
//! strongly, there and again a bar later (its autocorrelation, weighted
//! towards the tempos music is usually counted in), and the beats are the
//! one row of evenly spaced moments that lands on the most onsets, found by
//! dynamic programming
//! (D. Ellis, "Beat Tracking by Dynamic Programming", 2007). A row is
//! allowed to breathe a little, so a band that drifts is followed, and it
//! carries on through a break where the drums stop, so the grid does not
//! fall apart in the quiet bar before a chorus. Each beat is then moved onto
//! the hit it sits on, between two frames where the hit falls between them.
//!
//! A tempo the person types in is taken at their word: the beats are then
//! one steady row at that tempo, a hair either side of it, laid where it
//! lands on the most hits - the song counted all through the way it was
//! played to a click, through an intro or a break with no drums, where a
//! row found on its own follows whatever plays there; see [`steady`].
//!
//! Which beat is the first of a bar is told apart by what usually happens
//! there: the kick, and a change of chord. Every beat gets an *accent*, how
//! much it sounds like a bar's start, and the bar's phase is the one whose
//! beats score highest on average - for any number of beats in a bar,
//! without listening again; see [`downbeat`].
//!
//! The analysis streams: samples go in as they are decoded and only a few
//! numbers per frame are kept, a hundred-odd bytes for every second.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Result;
use crate::samples::{AudioDecoder, AudioOptions, SampleFormat};

/// The rate the sound is analysed at. A beat lives below a few kilohertz;
/// 22.05 kHz keeps everything up to 11 kHz and halves the work of 44.1.
pub const RATE: u32 = 22_050;

/// Samples in one analysis frame: 46 ms, long enough to tell a kick's
/// 50 Hz from a bass line's 100.
const FRAME: usize = 1024;

/// Samples from one frame to the next: 11.6 ms, the precision a beat is
/// found to before it is moved onto its hit.
const HOP: usize = 256;

/// Frames per second of sound.
const FPS: f64 = RATE as f64 / HOP as f64;

/// The slowest and the fastest tempo looked for, in beats per minute: the
/// range Beat Shake takes too.
pub const MIN_BPM: f64 = 40.0;
/// See [`MIN_BPM`].
pub const MAX_BPM: f64 = 240.0;

/// The tempo music is most often counted in, and how far either side of it
/// (in octaves) a tempo is still as likely: the weighting that decides
/// between a song at 90 and the same song counted at 180.
const PRIOR_BPM: f64 = 120.0;
const PRIOR_OCTAVES: f64 = 1.0;

/// The beats a bar is looked for after, for a tempo to be judged by: four,
/// and three for music in threes. The first is the farthest.
const BAR_BEATS: [usize; 2] = [4, 3];

/// How strictly the row of beats keeps its spacing. Higher holds the tempo
/// through more of a passage that does not support it; lower follows a
/// drifting band more closely, and wanders more.
const TIGHTNESS: f64 = 100.0;

/// How far a tempo typed in may be off the song's, either way, for the
/// steady row to find the song's: a tempo typed rounded - 94 for a song at
/// 94.24 - is a third of a percent out, and would drift a beat's length off
/// the hits over a long song if it were laid as typed.
const STEADY_RANGE: f64 = 0.03;

/// How much more the low bands' onsets count than the rest.
const LOW_WEIGHT: f32 = 1.0;

/// How strong a hit has to be, against the song's typical beat, for the
/// beat on it to be placed on it rather than carried by the grid.
const ANCHOR: f32 = 0.5;

/// How much stronger than the song's typical beat a hit has to be for the
/// beats on one side of it alone to place a beat on it: the first hit of a
/// drop. See [`place`].
const DROP: f32 = 2.0;

/// The top of the low bands, the drums': the GIF Visualizer's onset range.
const LOW_TOP_HZ: f64 = 260.0;

/// The top of the bass bands, the kick's alone: under a snare's body, so a
/// beat with the snare on it is not taken for one with the kick.
const BASS_TOP_HZ: f64 = 130.0;

/// The range the chord is read in: below it a frame cannot tell semitones
/// apart, above it there is little but cymbals.
const TONE_LOW_HZ: f64 = 300.0;
const TONE_HIGH_HZ: f64 = 4000.0;

/// Band power that counts as silence: about 80 dB under a full-scale sine.
/// It keeps the logarithm of a quiet band from turning hiss into onsets.
const FLOOR: f32 = 1e-3;

/// How the band levels are kept: natural-log units in eighths, from the
/// floor up. A step is half a decibel.
const LEVEL_STEPS: f32 = 8.0;

/// A frame whose bands average under this many steps above the floor -
/// three decibels - is silence.
const SILENT: f32 = 6.0;

/// How long after the hit it is made by the onset curve's peak, which the
/// frames reach only once the hit is well inside them. Measured on the
/// synthetic hits of the tests, and taken off every beat.
const LATENCY: f64 = 0.0;

/// A radix-2 FFT of one size, its twiddles and its bit reversal worked out
/// once.
struct Fft {
    cos: Vec<f32>,
    sin: Vec<f32>,
    reverse: Vec<usize>,
}

impl Fft {
    fn new(size: usize) -> Fft {
        debug_assert!(size.is_power_of_two());
        let bits = size.trailing_zeros();
        let reverse = (0..size)
            .map(|index| index.reverse_bits() >> (usize::BITS - bits))
            .collect();
        let (cos, sin) = (0..size / 2)
            .map(|k| {
                let angle = -2.0 * std::f64::consts::PI * k as f64 / size as f64;
                (angle.cos() as f32, angle.sin() as f32)
            })
            .unzip();
        Fft { cos, sin, reverse }
    }

    /// Transforms `re` and `im` in place.
    fn run(&self, re: &mut [f32], im: &mut [f32]) {
        let size = re.len();
        for index in 0..size {
            let other = self.reverse[index];
            if other > index {
                re.swap(index, other);
                im.swap(index, other);
            }
        }
        let mut span = 2;
        while span <= size {
            let half = span / 2;
            let step = size / span;
            for start in (0..size).step_by(span) {
                for k in 0..half {
                    let (c, s) = (self.cos[k * step], self.sin[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            span *= 2;
        }
    }
}

/// What the analysis found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Analysis {
    /// Beats per minute, from the middle of the beats' spacing. Zero when no
    /// beat was found.
    pub tempo: f64,
    /// When each beat falls, in seconds from the start of the sound, in
    /// order.
    pub beats: Vec<f64>,
    /// How much each beat sounds like the first of a bar, one per beat: a
    /// score around zero, higher for a beat with the kick and a change of
    /// chord on it. See [`downbeat`].
    pub accents: Vec<f32>,
    /// How sure the beats are, 0 to 1: the share of them that sit on a hit
    /// in the sound rather than being carried through a quiet stretch.
    pub confidence: f32,
}

pub use concat_core::beats::downbeat;

/// The streaming analyser: samples in with [`Detector::push`], the beats
/// out of [`Detector::finish`].
pub struct Detector {
    fft: Fft,
    window: Vec<f32>,
    /// Samples waiting for a whole frame. Starts with half a frame of
    /// silence, so the first frame is centred on the first sample.
    pending: Vec<f32>,
    /// For every bin, the band it is summed into, or `usize::MAX`.
    band_of: Vec<usize>,
    bands: usize,
    /// Bands `0..low_bands` are the drums'.
    low_bands: usize,
    /// Bands `0..bass_bands` are the kick's alone.
    bass_bands: usize,
    /// The bins the chord is read from, with their pitch class.
    tones: Vec<(usize, usize)>,
    /// Band levels one and two frames back: a frame's onset is measured
    /// against the one two before, so a hit that grows over a frame is
    /// still one jump.
    previous: [Vec<f32>; 2],
    seen: usize,
    features: Features,
    re: Vec<f32>,
    im: Vec<f32>,
    levels: Vec<f32>,
}

/// What is kept of every frame.
#[derive(Default)]
struct Features {
    /// All bands' rise, the low bands' counted again.
    onset: Vec<f32>,
    /// The bass bands' rise alone: the kick.
    bass: Vec<f32>,
    /// Band levels, `bands` per frame, in [`LEVEL_STEPS`] above the floor.
    levels: Vec<u8>,
    /// The frame's share of tone in each of the twelve pitch classes, in
    /// 255ths.
    chroma: Vec<[u8; 12]>,
    /// The power the chroma was read from, to weigh frames by.
    tone: Vec<f32>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    /// A detector waiting for mono samples at [`RATE`].
    pub fn new() -> Detector {
        let window = (0..FRAME)
            .map(|index| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * index as f64 / FRAME as f64).cos())
                    as f32
            })
            .collect();
        // Third-octave bands from 40 Hz, numbered from there; the rumble
        // under 40 Hz is left out, and a band no bin falls in is skipped.
        let hz = |bin: usize| bin as f64 * RATE as f64 / FRAME as f64;
        let mut band_of = vec![usize::MAX; FRAME / 2 + 1];
        let mut numbers: Vec<i64> = Vec::new();
        for (bin, slot) in band_of.iter_mut().enumerate() {
            let frequency = hz(bin);
            if frequency < 40.0 {
                continue;
            }
            let number = (3.0 * (frequency / 40.0).log2()).floor() as i64;
            if numbers.last() != Some(&number) {
                numbers.push(number);
            }
            *slot = numbers.len() - 1;
        }
        let bands = numbers.len();
        let below = |top: f64| {
            numbers
                .iter()
                .filter(|number| 40.0 * 2f64.powf((**number + 1) as f64 / 3.0) <= top + 1.0)
                .count()
        };
        let (low_bands, bass_bands) = (below(LOW_TOP_HZ), below(BASS_TOP_HZ));
        let tones = (0..=FRAME / 2)
            .filter(|bin| (TONE_LOW_HZ..=TONE_HIGH_HZ).contains(&hz(*bin)))
            .map(|bin| {
                let note = (12.0 * (hz(bin) / 440.0).log2()).round() as i64 + 69;
                (bin, note.rem_euclid(12) as usize)
            })
            .collect();
        Detector {
            fft: Fft::new(FRAME),
            window,
            pending: vec![0.0; FRAME / 2],
            band_of,
            bands,
            low_bands,
            bass_bands,
            tones,
            previous: [vec![FLOOR.ln(); bands], vec![FLOOR.ln(); bands]],
            seen: 0,
            features: Features::default(),
            re: vec![0.0; FRAME],
            im: vec![0.0; FRAME],
            levels: vec![0.0; bands],
        }
    }

    /// Takes the next mono samples at [`RATE`], in [-1, 1].
    pub fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        self.drain();
    }

    /// Seconds of sound taken so far.
    pub fn seconds(&self) -> f64 {
        self.seen as f64 / FPS
    }

    fn drain(&mut self) {
        let mut start = 0;
        while self.pending.len() - start >= FRAME {
            self.frame(start);
            start += HOP;
        }
        self.pending.drain(..start);
    }

    fn frame(&mut self, start: usize) {
        for index in 0..FRAME {
            self.re[index] = self.pending[start + index] * self.window[index];
            self.im[index] = 0.0;
        }
        self.fft.run(&mut self.re, &mut self.im);
        self.levels.iter_mut().for_each(|level| *level = 0.0);
        let mut chroma = [0.0f32; 12];
        let mut tone = 0.0f32;
        let mut tone_at = self.tones.iter().peekable();
        for bin in 1..=FRAME / 2 {
            let power = self.re[bin] * self.re[bin] + self.im[bin] * self.im[bin];
            let band = self.band_of[bin];
            if band != usize::MAX {
                self.levels[band] += power;
            }
            if let Some((tone_bin, class)) = tone_at.peek()
                && *tone_bin == bin
            {
                chroma[*class] += power;
                tone += power;
                tone_at.next();
            }
        }
        let floor = FLOOR.ln();
        let mut rise = 0.0f32;
        let mut low = 0.0f32;
        let mut bass = 0.0f32;
        for band in 0..self.bands {
            let level = (self.levels[band] + FLOOR).ln();
            self.levels[band] = level;
            let grown = (level - self.previous[0][band]).max(0.0);
            rise += grown;
            if band < self.low_bands {
                low += grown;
            }
            if band < self.bass_bands {
                bass += grown;
            }
            self.features
                .levels
                .push(((level - floor) * LEVEL_STEPS).round().clamp(0.0, 255.0) as u8);
        }
        let rise = rise / self.bands as f32;
        let low = low / self.low_bands.max(1) as f32;
        self.features.onset.push(rise + LOW_WEIGHT * low);
        self.features
            .bass
            .push(bass / self.bass_bands.max(1) as f32);
        let share = if tone > 0.0 { 255.0 / tone } else { 0.0 };
        self.features
            .chroma
            .push(chroma.map(|power| (power * share).round().clamp(0.0, 255.0) as u8));
        self.features.tone.push(tone);
        // Two back becomes one back, and this frame the one before.
        self.previous.swap(0, 1);
        self.previous[1].copy_from_slice(&self.levels);
        self.seen += 1;
    }

    /// Ends the sound and finds its beats, and their tempo.
    pub fn finish(self) -> Analysis {
        self.finish_at(None)
    }

    /// Ends the sound and finds its beats: at the tempo found in it, or,
    /// when `tempo` is given - the person's word for what the song is in,
    /// in beats per minute - as one steady row at about that tempo, laid
    /// where it lands on the most hits, through the intro and the breaks
    /// the drums leave out. See [`steady`].
    pub fn finish_at(mut self, tempo: Option<f64>) -> Analysis {
        // Half a frame of silence after the last sample, so the last frames
        // are centred on the end as the first were on the start.
        self.pending.extend(std::iter::repeat_n(0.0, FRAME / 2));
        self.drain();
        track(&self.features, self.bands, tempo)
    }
}

/// Decodes the audio stream `stream` names (or the file's first) and finds
/// its beats, at `tempo` beats per minute when that is given (see
/// [`Detector::finish_at`]). `duration` is the sound's length, when it is
/// known, for `progress`, which is told the fraction done, 0 to 1. Stops
/// between two decoded chunks once `cancel` is set, and is then `None`.
pub fn analyse_file(
    path: &Path,
    stream: Option<usize>,
    duration: Option<f64>,
    tempo: Option<f64>,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<Option<Analysis>> {
    let mut decoder = AudioDecoder::open(
        path,
        &AudioOptions {
            rate: RATE,
            channels: 1,
            format: SampleFormat::F32,
            stream,
            ..AudioOptions::default()
        },
    )?;
    let total = duration
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| seconds * f64::from(RATE));
    let mut detector = Detector::new();
    let mut decoded = 0usize;
    while let Some(chunk) = decoder.next_f32()? {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        detector.push(&chunk);
        decoded += chunk.len();
        if let Some(total) = total {
            // The decode is nearly all of it; the rest is the last step.
            progress((decoded as f64 / total).min(1.0) as f32 * 0.97);
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let analysis = detector.finish_at(tempo);
    progress(1.0);
    Ok(Some(analysis))
}

/// From the frames to the beats: at the tempo the curve repeats at, or as
/// a steady row at about `tempo` beats per minute when one is typed in.
fn track(features: &Features, bands: usize, tempo: Option<f64>) -> Analysis {
    let frames = features.onset.len();
    // Two seconds is the least that holds a few beats at the slowest tempo
    // looked for; less is no tempo at all.
    if frames < (2.0 * FPS) as usize {
        return Analysis::default();
    }
    let curve = condition(&features.onset);
    let typed = tempo
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        // A beat of at least two frames and at most half the sound: what
        // a row can be counted in at all.
        .map(|bpm| (60.0 * FPS / bpm).clamp(2.0, (frames / 2).max(2) as f64));
    let (mut beats, on_hits, period) = match typed {
        Some(near) => {
            let (length, first) = steady(&curve, near);
            let (period, first) = refine(&features.onset, length, first);
            let beats: Vec<f64> = (0..)
                .map(|index| first + index as f64 * period)
                .take_while(|frame| *frame <= (frames - 1) as f64)
                .collect();
            let on_hits = hits_under(&features.onset, &beats);
            (beats, on_hits, period)
        }
        None => {
            let Some(period) = period(&curve) else {
                return Analysis::default();
            };
            let found = dynamic(&curve, period);
            if found.is_empty() {
                return Analysis::default();
            }
            let (beats, on_hits) = place(&features.onset, &found, period);
            (beats, on_hits, period)
        }
    };
    // The row can reach a beat into the silence before the music starts or
    // after it ends; there is nothing there to count.
    let quiet = |beat: f64| {
        let reach = period / 4.0;
        let from = (beat - reach).max(0.0) as usize;
        let to = ((beat + reach) as usize).min(frames - 1);
        (from..=to).all(|frame| {
            let levels = &features.levels[frame * bands..(frame + 1) * bands];
            let sum: u32 = levels.iter().map(|level| u32::from(*level)).sum();
            (sum as f32 / bands as f32) < SILENT
        })
    };
    while beats.first().is_some_and(|beat| quiet(*beat)) {
        beats.remove(0);
    }
    while beats.last().is_some_and(|beat| quiet(*beat)) {
        beats.pop();
    }
    if beats.is_empty() {
        return Analysis::default();
    }
    let accents = accents(features, bands, &beats);
    // A steady row is its spacing; a found one breathes, and its tempo is
    // the middle of its gaps.
    let tempo = if typed.is_some() {
        60.0 * FPS / period
    } else {
        tempo_of(&beats).unwrap_or(60.0 * FPS / period)
    };
    let confidence = (on_hits as f32 / beats.len() as f32).min(1.0);
    Analysis {
        tempo,
        beats: beats
            .iter()
            .map(|frame| (frame / FPS - LATENCY).max(0.0))
            .collect(),
        accents,
        confidence,
    }
}

/// The steady row at about `near` frames a beat that lands on the most of
/// the curve: its beat's length, within [`STEADY_RANGE`] of `near`, and
/// where its first beat falls, both in frames. The curve is folded onto
/// one beat - every frame added in at the place in the beat it falls at -
/// and the row goes where the fold piles highest: first in steps of a
/// length the row drifts half a frame over the sound by, then finely
/// round the best of them, a quarter of a frame to the place.
fn steady(curve: &[f32], near: f64) -> (f64, f64) {
    const PLACES: f64 = 4.0;
    let frames = curve.len() as f64;
    let beats = (frames / near).max(1.0);
    // Where in a beat of `length` the curve piles highest, counted `width`
    // frames wide, and how high: the place a row's beats fall at.
    let fold = |length: f64, width: f64| -> (f64, f64) {
        let places = (length * PLACES).ceil().max(1.0) as usize;
        let mut pile = vec![0.0f64; places];
        for (frame, value) in curve.iter().enumerate() {
            if *value > 0.0 {
                let place = (frame as f64).rem_euclid(length) * PLACES;
                pile[(place as usize).min(places - 1)] += f64::from(*value);
            }
        }
        let half = ((width * PLACES / 2.0).round() as usize).min((places - 1) / 2);
        let mut best = (f64::NEG_INFINITY, 0usize);
        for middle in 0..places {
            let sum: f64 = (0..=2 * half)
                .map(|step| pile[(middle + places + step - half) % places])
                .sum();
            if sum > best.0 {
                best = (sum, middle);
            }
        }
        (best.0, (best.1 as f64 + 0.5) / PLACES)
    };
    let range = STEADY_RANGE * near;
    // No more than two thousand steps, for an hour of sound.
    let coarse = (1.0 / beats).max(2.0 * range / 2000.0);
    let search = |from: f64, to: f64, step: f64, width: f64| {
        let mut best = (f64::NEG_INFINITY, near, 0.0);
        let mut length = from;
        while length <= to {
            let (pile, place) = fold(length, width);
            if pile > best.0 {
                best = (pile, length, place);
            }
            length += step;
        }
        best
    };
    let (_, rough, _) = search(near - range, near + range, coarse, 2.0);
    let (_, length, first) = search(rough - coarse, rough + coarse, coarse / 10.0, 1.0);
    // The last place's middle can lie a hair past the beat's end.
    (length, first.rem_euclid(length))
}

/// The steady row `length` frames a beat from `first` moved onto the hits
/// it lands on: a line through the strong hits' own places, each between
/// two frames by its peak's shape as [`place`] has it, fitted by least
/// squares, then again without the hits more than a frame and a half off
/// it. The fold places a row to a quarter of a frame, and its length so
/// that the row is never more than a frame off; the hits put it within a
/// millisecond or two of every one of them. The row as it came when too few
/// hits are strong, or when they would move it more than a percent.
fn refine(onset: &[f32], length: f64, first: f64) -> (f64, f64) {
    let reach = (0.035 * FPS).round() as isize;
    let last = onset.len() as isize - 1;
    let mut hits: Vec<(f64, f64, f32)> = Vec::new();
    let mut index = 0usize;
    loop {
        let beat = first + index as f64 * length;
        if beat > last as f64 {
            break;
        }
        let at = beat.round() as isize;
        let (from, to) = ((at - reach).max(0), (at + reach).min(last));
        if let Some(peak) =
            (from..=to).max_by(|a, b| onset[*a as usize].total_cmp(&onset[*b as usize]))
        {
            let peak = peak as usize;
            let mut place = peak as f64;
            if peak > 0 && (peak as isize) < last {
                let (before, at, after) = (
                    f64::from(onset[peak - 1]),
                    f64::from(onset[peak]),
                    f64::from(onset[peak + 1]),
                );
                let bend = before - 2.0 * at + after;
                if bend < 0.0 {
                    place += (0.5 * (before - after) / bend).clamp(-0.5, 0.5);
                }
            }
            hits.push((index as f64, place, onset[peak]));
        }
        index += 1;
    }
    let mut strengths: Vec<f32> = hits.iter().map(|(_, _, strength)| *strength).collect();
    strengths.sort_by(f32::total_cmp);
    let typical = strengths.get(strengths.len() / 2).copied().unwrap_or(0.0);
    let mut kept: Vec<(f64, f64)> = hits
        .iter()
        .filter(|(_, _, strength)| typical > 0.0 && *strength >= ANCHOR * typical)
        .map(|(index, place, _)| (*index, *place))
        .collect();
    let mut row = (length, first);
    for _ in 0..3 {
        if kept.len() < 4 {
            return (length, first);
        }
        let count = kept.len() as f64;
        let mean_x = kept.iter().map(|(x, _)| x).sum::<f64>() / count;
        let mean_y = kept.iter().map(|(_, y)| y).sum::<f64>() / count;
        let spread: f64 = kept.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
        if spread <= 0.0 {
            return (length, first);
        }
        let slope = kept
            .iter()
            .map(|(x, y)| (x - mean_x) * (y - mean_y))
            .sum::<f64>()
            / spread;
        row = (slope, mean_y - slope * mean_x);
        kept.retain(|(x, y)| (y - (row.1 + row.0 * x)).abs() <= 1.5);
    }
    if (row.0 / length - 1.0).abs() > 0.01 {
        return (length, first);
    }
    // The first beat may now sit a whole beat before the sound.
    let first = row.1.rem_euclid(row.0);
    (row.0, first)
}

/// How many of `beats`, frames, have a hit on them as strong as the song's
/// beats are, as [`place`] counts its anchors: for a steady row, which is
/// not moved onto its hits.
fn hits_under(onset: &[f32], beats: &[f64]) -> usize {
    let reach = (0.035 * FPS).round() as usize;
    let peaks: Vec<f32> = beats
        .iter()
        .map(|beat| {
            let at = (beat.round().max(0.0) as usize).min(onset.len() - 1);
            let from = at.saturating_sub(reach);
            let to = (at + reach).min(onset.len() - 1);
            onset[from..=to].iter().copied().fold(0.0, f32::max)
        })
        .collect();
    let mut sorted = peaks.clone();
    sorted.sort_by(f32::total_cmp);
    let typical = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    if typical <= 0.0 {
        return 0;
    }
    peaks
        .iter()
        .filter(|peak| **peak >= ANCHOR * typical)
        .count()
}

/// Sums of `values` up to each index, for windowed means in constant time.
fn prefix(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut sums = vec![0.0];
    let mut total = 0.0;
    for value in values {
        total += value;
        sums.push(total);
    }
    sums
}

/// The mean of `sums`' values over `index - reach ..= index + reach`, as
/// far as there are values.
fn window_mean(sums: &[f64], index: usize, reach: usize) -> f64 {
    let count = sums.len() - 1;
    let from = index.saturating_sub(reach);
    let to = (index + reach + 1).min(count);
    (sums[to] - sums[from]) / (to - from).max(1) as f64
}

/// The onset curve made fit to count along: its slow swell taken off, so
/// only the hits stand up, and each stretch scaled by how busy the music
/// around it is, so the quiet verse counts as much as the loud chorus - the
/// range that follows the music, as the GIF Visualizer's level has it. A
/// stretch of near silence is not blown up into hits: the scale never
/// falls below a fraction of the whole song's.
fn condition(onset: &[f32]) -> Vec<f32> {
    let swell = prefix(onset.iter().map(|value| f64::from(*value)));
    let reach = (0.5 * FPS) as usize;
    let lifted: Vec<f32> = onset
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (f64::from(*value) - window_mean(&swell, index, reach)).max(0.0) as f32
        })
        .collect();
    let squares = prefix(lifted.iter().map(|value| f64::from(*value).powi(2)));
    let whole = window_mean(&squares, 0, lifted.len()).sqrt();
    if whole <= 0.0 {
        return lifted;
    }
    let around = (3.0 * FPS) as usize;
    lifted
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let scale = window_mean(&squares, index, around).sqrt().max(0.3 * whole);
            (f64::from(*value) / scale) as f32
        })
        .collect()
}

/// The beat's length in frames, between one and the next. Every lag the
/// curve repeats at is a candidate, scored by how strongly it repeats there
/// and a bar later - four beats on, or three for music in threes - weighted
/// towards [`PRIOR_BPM`]; the winner is placed between two frames by the
/// shape of its peak. The bar is what tells a beat from three quarters of
/// one: music that runs in sixteenths repeats nearly as well every three of
/// them as every four, but its bars come round every sixteen, not every
/// twelve. None for a curve with no repetition in the range at all.
fn period(curve: &[f32]) -> Option<f64> {
    let frames = curve.len();
    let shortest = ((60.0 * FPS / MAX_BPM).floor() as usize).max(2);
    let longest = ((60.0 * FPS / MIN_BPM).ceil() as usize).min(frames / 2);
    if longest <= shortest + 1 {
        return None;
    }
    let lagged = |lag: usize| -> f64 {
        let sum: f64 = curve[..frames - lag]
            .iter()
            .zip(&curve[lag..])
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        sum / (frames - lag) as f64
    };
    let zero = lagged(0);
    if zero <= 0.0 {
        return None;
    }
    // Far enough for the bar of the slowest beat, as far as the sound holds.
    let reach = (BAR_BEATS[0] * longest + BAR_BEATS[0])
        .min(frames / 2)
        .max(longest + 1);
    let correlation: Vec<f64> = (0..=reach).map(|lag| lagged(lag) / zero).collect();
    let weight = |lag: f64| {
        let octaves = (60.0 * FPS / lag / PRIOR_BPM).log2() / PRIOR_OCTAVES;
        (-0.5 * octaves * octaves).exp()
    };
    // The strongest repetition within half a frame per beat of `beats`
    // beats of `lag`: a beat half a frame off its length is two frames off
    // by the end of a bar. None past what the sound holds.
    let bar_at = |lag: usize, beats: usize| -> Option<f64> {
        let middle = (lag * beats) as f64;
        let slack = beats as f64 / 2.0;
        let from = (middle - slack).floor() as usize;
        let to = (middle + slack).ceil() as usize;
        (to < correlation.len()).then(|| {
            correlation[from..=to]
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max)
        })
    };
    let score = |lag: usize| {
        let beat = correlation[lag];
        let bar = BAR_BEATS
            .iter()
            .filter_map(|beats| bar_at(lag, *beats))
            .reduce(f64::max)
            .unwrap_or(beat);
        weight(lag as f64) * (beat + bar) / 2.0
    };
    // The lags the curve peaks at; a beat has to repeat itself, not only
    // land a bar on something that does.
    let peaks: Vec<usize> = (shortest..=longest)
        .filter(|&lag| {
            correlation[lag] > 0.0
                && correlation[lag] >= correlation[lag - 1]
                && correlation[lag] >= correlation[lag + 1]
        })
        .collect();
    let candidates: Vec<usize> = if peaks.is_empty() {
        (shortest..=longest).collect()
    } else {
        peaks
    };
    let best = candidates
        .into_iter()
        .max_by(|a, b| score(*a).total_cmp(&score(*b)))?;
    if correlation[best] <= 0.0 {
        return None;
    }
    let (before, at, after) = (
        correlation[best - 1],
        correlation[best],
        correlation[best + 1],
    );
    let bend = before - 2.0 * at + after;
    let shift = if bend < 0.0 {
        (0.5 * (before - after) / bend).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some(best as f64 + shift)
}

/// The row of beats, as frames: the evenly spaced row, give or take, that
/// lands on the most of the curve. Ellis' dynamic programme, as librosa
/// has it: every frame's best score as the last beat so far is its own
/// onset plus the best of the frames half a beat to two beats back, less
/// what it costs to have strayed from the beat's length; the last beat is
/// the last frame that scores well, and the rest are followed back from it.
fn dynamic(curve: &[f32], period: f64) -> Vec<usize> {
    let frames = curve.len();
    // The curve over its spread, so a beat is drawn to the middle of a hit
    // and not to its edge.
    let spread = period.round() as isize;
    let kernel: Vec<f64> = (-spread..=spread)
        .map(|offset| (-0.5 * (offset as f64 * 32.0 / period).powi(2)).exp())
        .collect();
    let deviation = {
        let mean = curve.iter().map(|value| f64::from(*value)).sum::<f64>() / frames as f64;
        let variance = curve
            .iter()
            .map(|value| (f64::from(*value) - mean).powi(2))
            .sum::<f64>()
            / frames as f64;
        variance.sqrt().max(1e-9)
    };
    let local: Vec<f64> = (0..frames as isize)
        .map(|index| {
            kernel
                .iter()
                .enumerate()
                .filter_map(|(k, weight)| {
                    let at = index + k as isize - spread;
                    (0..frames as isize)
                        .contains(&at)
                        .then(|| weight * f64::from(curve[at as usize]) / deviation)
                })
                .sum()
        })
        .collect();

    let nearest = ((period / 2.0).round() as usize).max(1);
    let farthest = ((2.0 * period).round() as usize).max(nearest + 1);
    let cost: Vec<f64> = (nearest..=farthest)
        .map(|gap| -TIGHTNESS * (gap as f64 / period).ln().powi(2))
        .collect();
    let threshold = 0.01 * local.iter().cloned().fold(0.0, f64::max);
    let mut score = vec![0.0f64; frames];
    let mut back: Vec<Option<usize>> = vec![None; frames];
    let mut started = false;
    for index in 0..frames {
        // The best predecessor; a gap reaching back before the start costs
        // only its stray, as if a beat stood there with nothing to it.
        let mut best = (f64::NEG_INFINITY, None);
        for (offset, stray) in cost.iter().enumerate() {
            let gap = nearest + offset;
            let candidate = if gap <= index {
                (score[index - gap] + stray, Some(index - gap))
            } else {
                (*stray, None)
            };
            if candidate.0 > best.0 {
                best = candidate;
            }
        }
        score[index] = local[index] + best.0;
        if !started && local[index] < threshold {
            back[index] = None;
        } else {
            started = true;
            back[index] = best.1;
        }
    }
    if !started {
        return Vec::new();
    }

    // The last beat: the last peak of the running score that is not far
    // under the peaks' middle.
    let peaks: Vec<usize> = (0..frames)
        .filter(|&index| {
            let left = index == 0 || score[index] > score[index - 1];
            let right = index + 1 == frames || score[index] >= score[index + 1];
            left && right
        })
        .collect();
    if peaks.is_empty() {
        return Vec::new();
    }
    let mut values: Vec<f64> = peaks.iter().map(|&index| score[index]).collect();
    values.sort_by(f64::total_cmp);
    let middle = values[values.len() / 2];
    let Some(last) = peaks
        .iter()
        .rev()
        .find(|&&index| 2.0 * score[index] > middle)
        .copied()
    else {
        return Vec::new();
    };
    let mut beats = vec![last];
    while let Some(previous) = back[*beats.last().unwrap_or(&0)] {
        beats.push(previous);
    }
    beats.reverse();

    // A row carried into the silence before the music or after it is not
    // beats of the music: those at either end that land on nothing are cut.
    let at_beats: Vec<f64> = beats.iter().map(|&frame| local[frame]).collect();
    let smooth: Vec<f64> = (0..at_beats.len())
        .map(|index| {
            let hann = [0.0, 0.5, 1.0, 0.5, 0.0];
            hann.iter()
                .enumerate()
                .filter_map(|(k, weight)| {
                    (index + k)
                        .checked_sub(2)
                        .and_then(|at| at_beats.get(at))
                        .map(|value| weight * value)
                })
                .sum()
        })
        .collect();
    let cut =
        0.5 * (smooth.iter().map(|value| value * value).sum::<f64>() / smooth.len() as f64).sqrt();
    let first = smooth.iter().position(|value| *value > cut);
    let final_ = smooth.iter().rposition(|value| *value > cut);
    match (first, final_) {
        (Some(first), Some(final_)) => beats[first..=final_].to_vec(),
        _ => Vec::new(),
    }
}

/// Every beat moved onto the hit it sits on, placed between frames by the
/// peak's shape - when that hit is as strong as the song's beats are: such
/// a beat is an *anchor*. A beat with no such hit, in a break where only a
/// voice or a pad goes on, would be pulled about by whatever it is near;
/// it is instead spaced evenly between the anchors either side of it, the
/// grid carried on through the break the way a drummer counts it. Also how
/// many are anchors. In frames, fractional.
fn place(onset: &[f32], beats: &[usize], period: f64) -> (Vec<f64>, usize) {
    let reach = (0.035 * FPS).round() as usize;
    let peaks: Vec<(f64, f32)> = beats
        .iter()
        .map(|&beat| {
            let from = beat.saturating_sub(reach);
            let to = (beat + reach).min(onset.len() - 1);
            let peak = (from..=to)
                .max_by(|a, b| onset[*a].total_cmp(&onset[*b]))
                .unwrap_or(beat);
            if peak == 0 || peak + 1 >= onset.len() {
                return (peak as f64, onset[peak]);
            }
            let (before, at, after) = (
                f64::from(onset[peak - 1]),
                f64::from(onset[peak]),
                f64::from(onset[peak + 1]),
            );
            let bend = before - 2.0 * at + after;
            let shift = if bend < 0.0 {
                (0.5 * (before - after) / bend).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            (peak as f64 + shift, onset[peak])
        })
        .collect();
    let mut strengths: Vec<f32> = peaks.iter().map(|(_, strength)| *strength).collect();
    strengths.sort_by(f32::total_cmp);
    let typical = strengths.get(strengths.len() / 2).copied().unwrap_or(0.0);
    let strong: Vec<usize> = (0..beats.len())
        .filter(|&index| typical > 0.0 && peaks[index].1 >= ANCHOR * typical)
        .collect();
    // A strong hit off the grid - a sung syllable in the break, a fill - is
    // not an anchor either: each is checked against the line its strong
    // neighbours, four either side, draw through the beats.
    let tolerance = (0.035 * FPS).max(0.08 * period);
    // Where the line through `points`, (beat, frame), falls at beat `index`:
    // none through fewer than two.
    let line_at = |points: &[(f64, f64)], index: f64| -> Option<f64> {
        if points.len() < 2 {
            return None;
        }
        let count = points.len() as f64;
        let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / count;
        let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / count;
        let spread: f64 = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
        if spread <= 0.0 {
            return None;
        }
        let slope = points
            .iter()
            .map(|(x, y)| (x - mean_x) * (y - mean_y))
            .sum::<f64>()
            / spread;
        Some(mean_y + slope * (index - mean_x))
    };
    let point = |other: usize| (strong[other] as f64, peaks[strong[other]].0);
    let fits = |at: usize, expected: f64| (peaks[strong[at]].0 - expected).abs() <= tolerance;
    let on_line: Vec<bool> = (0..strong.len())
        .map(|at| {
            let from = at.saturating_sub(4);
            let to = (at + 5).min(strong.len());
            let points: Vec<(f64, f64)> =
                (from..to).filter(|&other| other != at).map(point).collect();
            line_at(&points, strong[at] as f64).is_none_or(|expected| fits(at, expected))
        })
        .collect();
    // A second look at a hit the line missed that is far stronger than the
    // song's beats - the first hit of a drop, a crash on the one: it may
    // carry on the line the anchors on one side of it draw, alone. After an
    // intro the row ran off the beat in, one line through both sides misses
    // the drop's first hit. A run of hits off the beat together - a
    // syncopated bar - does not get in: its neighbours on the line are the
    // anchors, and it is no louder than the beats.
    let anchors: Vec<usize> = (0..strong.len())
        .filter(|&at| {
            if on_line[at] {
                return true;
            }
            if peaks[strong[at]].1 < DROP * typical {
                return false;
            }
            let near = |others: &mut dyn Iterator<Item = usize>| -> Vec<(f64, f64)> {
                others
                    .filter(|&other| on_line[other])
                    .take(4)
                    .map(point)
                    .collect()
            };
            let before = near(&mut (at.saturating_sub(8)..at).rev());
            let after = near(&mut (at + 1..(at + 9).min(strong.len())));
            let index = strong[at] as f64;
            [line_at(&before, index), line_at(&after, index)]
                .into_iter()
                .flatten()
                .any(|expected| fits(at, expected))
        })
        .map(|at| strong[at])
        .collect();
    if anchors.is_empty() {
        return (beats.iter().map(|beat| *beat as f64).collect(), 0);
    }
    let mut placed: Vec<f64> = beats.iter().map(|beat| *beat as f64).collect();
    for &anchor in &anchors {
        placed[anchor] = peaks[anchor].0;
    }
    // Between two anchors: evenly, when the row's count of beats between
    // them fits their distance. A count that does not fit - a beat the row
    // dropped or doubled - keeps the row's own places.
    for pair in anchors.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b - a < 2 {
            continue;
        }
        let spacing = (placed[b] - placed[a]) / (b - a) as f64;
        if (spacing / period - 1.0).abs() > 0.25 {
            continue;
        }
        for index in a + 1..b {
            placed[index] = placed[a] + spacing * (index - a) as f64;
        }
    }
    // Before the first anchor and after the last, at the beat's length,
    // counted from the anchor.
    let first = anchors[0];
    for index in 0..first {
        placed[index] = placed[first] - period * (first - index) as f64;
    }
    let last = anchors[anchors.len() - 1];
    for index in last + 1..beats.len() {
        placed[index] = placed[last] + period * (index - last) as f64;
    }
    let frames = onset.len() as f64;
    placed
        .iter_mut()
        .for_each(|frame| *frame = frame.clamp(0.0, frames - 1.0));
    (placed, anchors.len())
}

/// Beats per minute from the middle of the spacing of `beats` (frames).
fn tempo_of(beats: &[f64]) -> Option<f64> {
    let mut gaps: Vec<f64> = beats.windows(2).map(|pair| pair[1] - pair[0]).collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_by(f64::total_cmp);
    let middle = gaps[gaps.len() / 2];
    (middle > 0.0).then(|| 60.0 * FPS / middle)
}

/// How much each beat sounds like the first of a bar: the kick on it, and
/// how much the chord and the sound changed from the beat before to it,
/// each as a z-score over the song.
fn accents(features: &Features, bands: usize, beats: &[f64]) -> Vec<f32> {
    let frames = features.onset.len();
    let count = beats.len();
    if count == 0 {
        return Vec::new();
    }
    let at = |beat: f64| (beat.round().max(0.0) as usize).min(frames - 1);
    // The kick: the strongest bass rise just around the beat.
    let kick: Vec<f64> = beats
        .iter()
        .map(|beat| {
            let centre = at(*beat);
            let from = centre.saturating_sub(3);
            let to = (centre + 4).min(frames - 1);
            features.bass[from..=to]
                .iter()
                .cloned()
                .fold(0.0f32, f32::max)
                .into()
        })
        .collect();
    // From each beat to the next: the chord, weighed by how much tone each
    // frame has, and the band levels.
    let spans: Vec<(usize, usize)> = (0..count)
        .map(|index| {
            let from = at(beats[index]);
            let to = if index + 1 < count {
                at(beats[index + 1])
            } else {
                (from + (at(beats[index]) - at(beats[index.saturating_sub(1)])).max(1)).min(frames)
            };
            (from, to.max(from + 1).min(frames))
        })
        .collect();
    let chords: Vec<[f64; 12]> = spans
        .iter()
        .map(|&(from, to)| {
            let mut chord = [0.0f64; 12];
            for frame in from..to {
                let weight = f64::from(features.tone[frame]).sqrt();
                for (class, share) in features.chroma[frame].iter().enumerate() {
                    chord[class] += weight * f64::from(*share);
                }
            }
            let length = chord.iter().map(|value| value * value).sum::<f64>().sqrt();
            if length > 0.0 {
                chord.map(|value| value / length)
            } else {
                chord
            }
        })
        .collect();
    let colours: Vec<Vec<f64>> = spans
        .iter()
        .map(|&(from, to)| {
            let mut colour = vec![0.0f64; bands];
            for frame in from..to {
                for (band, level) in features.levels[frame * bands..(frame + 1) * bands]
                    .iter()
                    .enumerate()
                {
                    colour[band] += f64::from(*level) / LEVEL_STEPS as f64;
                }
            }
            let length = (to - from).max(1) as f64;
            colour.iter_mut().for_each(|value| *value /= length);
            colour
        })
        .collect();
    let chord_change: Vec<f64> = (0..count)
        .map(|index| {
            if index == 0 {
                return 0.0;
            }
            let similar: f64 = chords[index]
                .iter()
                .zip(&chords[index - 1])
                .map(|(a, b)| a * b)
                .sum();
            1.0 - similar
        })
        .collect();
    let colour_change: Vec<f64> = (0..count)
        .map(|index| {
            if index == 0 {
                return 0.0;
            }
            let distance: f64 = colours[index]
                .iter()
                .zip(&colours[index - 1])
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            (distance / bands.max(1) as f64).sqrt()
        })
        .collect();
    let kick = z_scores(&kick);
    let chord_change = z_scores(&chord_change);
    let colour_change = z_scores(&colour_change);
    (0..count)
        .map(|index| (kick[index] + chord_change[index] + 0.5 * colour_change[index]) as f32)
        .collect()
}

/// Each value as standard deviations from the values' mean; all zero when
/// they do not vary.
fn z_scores(values: &[f64]) -> Vec<f64> {
    let count = values.len().max(1) as f64;
    let mean = values.iter().sum::<f64>() / count;
    let deviation = (values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / count)
        .sqrt();
    if deviation <= 1e-12 {
        return vec![0.0; values.len()];
    }
    values
        .iter()
        .map(|value| (value - mean) / deviation)
        .collect()
}

#[cfg(test)]
#[path = "beats_tests.rs"]
mod tests;
