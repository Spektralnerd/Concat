// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! The phone's left-over copies, found and deleted, on folders made here.

use super::*;
use serde_json::json;

/// A phone's folders: the app's home with its copies and projects, and
/// its private settings, each with what the test puts in it.
struct Phone {
    _here: tempfile::TempDir,
    home: PathBuf,
    settings: PathBuf,
    imported: PathBuf,
}

impl Phone {
    fn new() -> Self {
        let here = tempfile::tempdir().expect("a scratch folder");
        let home = here.path().join("files");
        let settings = here.path().join("internal").join("app.concat.editor");
        let imported = folder(&home);
        for dir in [&home, &settings, &imported] {
            std::fs::create_dir_all(dir).expect("a folder");
        }
        Self {
            _here: here,
            home,
            settings,
            imported,
        }
    }

    /// A copy of `size` bytes in the copies' folder.
    fn copy(&self, name: &str, size: usize) -> PathBuf {
        let path = self.imported.join(name);
        std::fs::write(&path, vec![7u8; size]).expect("writes a copy");
        path
    }

    /// The path a project names a copy by, as the phone hands it over.
    fn named(&self, name: &str) -> String {
        self.imported.join(name).to_string_lossy().into_owned()
    }

    /// A project folder under the home, its document naming `media`.
    fn project(&self, folder: &str, media: &[String]) -> PathBuf {
        let root = self.home.join("Concat").join(folder);
        std::fs::create_dir_all(&root).expect("a project folder");
        std::fs::write(root.join("concat.json"), document(media).to_string())
            .expect("writes the document");
        root
    }

    fn search(&self) -> Search {
        Search {
            imported: self.imported.clone(),
            roots: vec![self.home.clone(), self.settings.clone()],
            skip: vec![self.settings.join("logs")],
            grace: Duration::ZERO,
            ..Search::default()
        }
    }
}

fn document(media: &[String]) -> Value {
    json!({
        "concat": "0.2.11",
        "name": "Fan edit",
        "media": media
            .iter()
            .enumerate()
            .map(|(index, path)| json!({ "id": format!("m{index}"), "path": path, "kind": "video" }))
            .collect::<Vec<_>>(),
        "timelines": [],
    })
}

fn names(leftovers: &Leftovers) -> Vec<String> {
    leftovers
        .files
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// What a project names is used and stays; what nothing names is left
/// over, with its bytes.
#[test]
fn a_copy_no_project_names_is_left_over() {
    let phone = Phone::new();
    phone.copy("film.mp4", 300);
    phone.copy("song.mp3", 40);
    phone.copy("subtitles.srt", 5);
    phone.project("Fan edit", &[phone.named("film.mp4")]);
    phone.project("Other", &[phone.named("song.mp3")]);

    let found = leftovers(&phone.search());
    assert_eq!(names(&found), vec!["subtitles.srt"]);
    assert_eq!(found.bytes, 5);
}

/// The open project is read as it stands in memory, not from its file: a
/// copy taken out of it is left over before the autosave has written, and
/// one just put in stays though the file does not name it yet.
#[test]
fn the_open_project_counts_as_it_is_in_memory() {
    let phone = Phone::new();
    phone.copy("taken-out.mp4", 10);
    phone.copy("just-added.mp4", 10);
    let root = phone.project("Fan edit", &[phone.named("taken-out.mp4")]);

    let mut search = phone.search();
    search.open = vec![document(&[phone.named("just-added.mp4")])];
    search.stale = vec![root.join("concat.json")];
    assert_eq!(names(&leftovers(&search)), vec!["taken-out.mp4"]);
}

/// Concat's own settings and libraries count, JSON and TOML alike; the
/// logs, which name every file ever picked, do not.
#[test]
fn settings_count_and_the_logs_do_not() {
    let phone = Phone::new();
    phone.copy("kept-by-a-template.mp3", 10);
    phone.copy("kept-by-settings.png", 10);
    phone.copy("only-in-the-log.mp4", 10);
    let templates = phone.settings.join("templates").join("intro");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("template.json"),
        json!({ "sound": phone.named("kept-by-a-template.mp3") }).to_string(),
    )
    .unwrap();
    std::fs::write(
        phone.settings.join("extra.toml"),
        format!("backdrop = \"{}\"\n", phone.named("kept-by-settings.png")),
    )
    .unwrap();
    let logs = phone.settings.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("concat-20261116.json"),
        json!({ "line": format!("import: copied {}", phone.named("only-in-the-log.mp4")) })
            .to_string(),
    )
    .unwrap();

    assert_eq!(
        names(&leftovers(&phone.search())),
        vec!["only-in-the-log.mp4"]
    );
}

/// A project of the recents list counts wherever it lies, outside every
/// folder searched.
#[test]
fn a_recent_project_elsewhere_counts() {
    let phone = Phone::new();
    phone.copy("clip.mp4", 10);
    let elsewhere = tempfile::tempdir().unwrap();
    let manifest = elsewhere.path().join("concat.json");
    std::fs::write(&manifest, document(&[phone.named("clip.mp4")]).to_string()).unwrap();

    let mut search = phone.search();
    assert_eq!(names(&leftovers(&search)), vec!["clip.mp4"]);
    search.files = vec![manifest];
    assert!(leftovers(&search).files.is_empty());
}

/// A name is looked for loosely - case and the slash's direction aside -
/// but a longer name is another file.
#[test]
fn a_name_is_matched_loosely_but_not_by_its_start_alone() {
    let phone = Phone::new();
    phone.copy("Song.MP3", 10);
    phone.copy("a.mp4", 10);
    let mut search = phone.search();
    search.open = vec![json!({
        "media": [
            { "path": "C:\\Phone\\IMPORTED\\song.mp3" },
            { "path": phone.named("a.mp4x") },
        ]
    })];
    assert_eq!(names(&leftovers(&search)), vec!["a.mp4"]);
}

/// A copy younger than the grace may be on its way into a project, and
/// stays; so does what is not a plain file right in the folder.
#[test]
fn young_copies_folders_and_links_are_not_left_over() {
    let phone = Phone::new();
    phone.copy("just-copied.mp4", 10);
    std::fs::create_dir_all(phone.imported.join("folder")).unwrap();
    std::fs::write(phone.imported.join("folder").join("inside.mp4"), b"x").unwrap();
    #[cfg(unix)]
    {
        let outside = phone.home.join("outside.mp4");
        std::fs::write(&outside, b"mine").unwrap();
        std::os::unix::fs::symlink(&outside, phone.imported.join("link.mp4")).unwrap();
    }

    let mut search = phone.search();
    search.grace = Duration::from_secs(3600);
    assert!(leftovers(&search).files.is_empty(), "young");
    search.grace = Duration::ZERO;
    assert_eq!(names(&leftovers(&search)), vec!["just-copied.mp4"]);
}

/// Only what was confirmed goes, and only while it is left over still: a
/// copy a project took up since stays, and so does one never asked about.
#[test]
fn delete_takes_what_was_confirmed_and_is_still_left_over() {
    let phone = Phone::new();
    let old = phone.copy("old.mp4", 100);
    let taken_up = phone.copy("taken-up.mp4", 50);
    let unasked = phone.copy("unasked.mp4", 25);
    let search = phone.search();
    let confirmed = vec![old.clone(), taken_up.clone()];
    assert_eq!(
        leftovers(&search).files,
        vec![old.clone(), taken_up.clone(), unasked.clone()]
    );

    // A project names one of them before the answer comes.
    phone.project("New", &[phone.named("taken-up.mp4")]);
    let deleted = delete(&search, &confirmed);
    assert_eq!(
        deleted,
        Deleted {
            files: 1,
            bytes: 100,
            failed: Vec::new()
        }
    );
    assert!(!old.exists());
    assert!(taken_up.exists());
    assert!(unasked.exists());
}

/// No copies folder - a desk, or a phone that never picked a file - has
/// nothing left over, and deletes nothing.
#[test]
fn no_folder_no_leftovers() {
    let here = tempfile::tempdir().unwrap();
    let search = Search {
        imported: here.path().join("Imported"),
        roots: vec![here.path().to_path_buf()],
        ..Search::default()
    };
    assert_eq!(leftovers(&search), Leftovers::default());
    assert_eq!(delete(&search, &[]), Deleted::default());
}
