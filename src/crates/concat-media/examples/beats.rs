//! Finds the beats of a file's sound and prints them: the detector as the
//! app runs it, for checking it on real songs.
//!
//! `cargo run -p concat-media --example beats -- song.mp3 [bpm]`

use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("a sound file");
    let tempo: Option<f64> = args.next().map(|text| text.parse().expect("a tempo"));
    let cancel = AtomicBool::new(false);
    let analysis = concat_media::beats::analyse_file(
        Path::new(&path),
        None,
        None,
        tempo,
        &mut |_| {},
        &cancel,
    )
    .expect("decodes")
    .expect("not cancelled");
    let first = concat_media::beats::downbeat(&analysis.accents, 4);
    println!(
        "tempo {:.2} BPM, {} beats, confidence {:.2}, bar starts at beat {}",
        analysis.tempo,
        analysis.beats.len(),
        analysis.confidence,
        first
    );
    let line: Vec<String> = analysis
        .beats
        .iter()
        .map(|beat| format!("{beat:.3}"))
        .collect();
    println!("beats {}", line.join(" "));
}
