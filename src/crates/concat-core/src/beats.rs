// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Beats: the one piece of arithmetic the beat finder in `concat-media`
//! and the beat grid a project keeps both need - which beat starts a bar.

/// Which of the beats, counted from the first, starts a bar of `per_bar`
/// beats, given how much each beat sounds like a bar's start (`accents`,
/// one per beat; see `concat_media::beats::Analysis::accents`): the phase
/// whose beats are, on average, the most accented. Zero when there are no
/// beats or a bar is one beat long.
pub fn downbeat(accents: &[f32], per_bar: usize) -> usize {
    if per_bar <= 1 || accents.is_empty() {
        return 0;
    }
    let mut best = (0, f32::NEG_INFINITY);
    for phase in 0..per_bar.min(accents.len()) {
        let (sum, count) = accents
            .iter()
            .skip(phase)
            .step_by(per_bar)
            .fold((0.0f32, 0usize), |(sum, count), accent| {
                (sum + accent, count + 1)
            });
        let mean = sum / count.max(1) as f32;
        if mean > best.1 {
            best = (phase, mean);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::downbeat;

    #[test]
    fn the_phase_is_the_most_accented_and_any_bar_length_works() {
        let accents = [0.1, 2.0, -0.5, 0.0, 0.2, 1.8, -0.4, 0.1, 0.0, 2.2];
        assert_eq!(downbeat(&accents, 4), 1);
        assert_eq!(downbeat(&accents, 1), 0);
        assert_eq!(downbeat(&[], 4), 0);
        // Three to a bar: phase 1 holds 2.0, 0.2 and 0.1, more on average
        // than phase 0's 0.1, 0.0, -0.4 and 2.2 or phase 2's -0.5, 1.8, 0.0.
        assert_eq!(downbeat(&accents, 3), 1);
        // More beats to a bar than there are beats: only the beats there are.
        assert_eq!(downbeat(&accents[..2], 4), 1);
    }
}
