// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! A project packed and opened again, on files made here.

use super::*;
use serde_json::json;

/// A project's document naming `media` and `fonts` by path.
fn document(name: &str, media: &[&str], fonts: &[&str]) -> Value {
    json!({
        "concat": "0.2.11",
        "name": name,
        "video": { "width": 1080, "height": 1920, "rateNum": 30, "rateDen": 1 },
        "media": media
            .iter()
            .enumerate()
            .map(|(index, path)| json!({
                "id": format!("m{}", index + 1),
                "path": path,
                "name": Path::new(path).file_name().unwrap().to_string_lossy(),
                "kind": "video",
                "hasAudio": true,
            }))
            .collect::<Vec<_>>(),
        "fonts": fonts
            .iter()
            .map(|path| json!({ "family": "Face", "path": path }))
            .collect::<Vec<_>>(),
        "timelines": [],
    })
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn names(archive: &Path) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut names: Vec<String> = (0..zip.len())
        .map(|index| zip.by_index(index).unwrap().name().unwrap().into_owned())
        .collect();
    names.sort();
    names
}

fn quiet() -> impl FnMut(u64, u64) {
    |_, _| {}
}

#[test]
fn a_project_packed_with_its_media_opens_again_on_its_own_files() {
    let here = tempfile::tempdir().unwrap();
    let a = here.path().join("phone/Imported/clip.mp4");
    let b = here.path().join("sd/clip.mp4");
    let song = here.path().join("phone/Imported/Amnesia.mp3");
    let face = here.path().join("fonts/Face.ttf");
    write(&a, b"first film");
    write(&b, b"second film, same name");
    write(&song, &vec![7u8; 3 * CHUNK + 17]);
    write(&face, b"glyphs");
    let gone = here.path().join("nowhere/lost.mov");
    let paths: Vec<String> = [&a, &b, &song, &gone, &a]
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let document = document(
        "Marty",
        &paths.iter().map(String::as_str).collect::<Vec<_>>(),
        &[&face.to_string_lossy()],
    );

    let out = here.path().join("Download/Marty.zip");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut said = Vec::new();
    let packed = export(
        &document,
        &out,
        true,
        &mut |done, total| said.push((done, total)),
        &AtomicBool::new(false),
    )
    .unwrap()
    .expect("not cancelled");
    assert_eq!(packed.files, 4, "the file used twice goes in once");
    assert_eq!(packed.missing, vec![gone.to_string_lossy().into_owned()]);
    assert_eq!(packed.bytes, 10 + 22 + (3 * CHUNK as u64 + 17) + 6);
    assert_eq!(said.last(), Some(&(packed.bytes, packed.bytes)));
    assert!(said.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert!(!partial_path(&out).exists());
    assert_eq!(
        names(&out),
        vec![
            "concat.json",
            "fonts/Face.ttf",
            "media/Amnesia.mp3",
            "media/clip (2).mp4",
            "media/clip.mp4",
        ]
    );

    let projects = here.path().join("other phone/Concat");
    std::fs::create_dir_all(&projects).unwrap();
    let info = import(&out, &projects, &mut quiet(), &AtomicBool::new(false))
        .unwrap()
        .expect("not cancelled");
    assert_eq!(info.name, "Marty");
    assert_eq!((info.width, info.height), (1080, 1920));
    let root = PathBuf::from(&info.path);
    assert_eq!(root, projects.join("Marty"));
    let opened = projects::read_document(&info.path).unwrap().unwrap();
    let media: Vec<String> = opened["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(media[0], root.join("media/clip.mp4").to_string_lossy());
    assert_eq!(media[1], root.join("media/clip (2).mp4").to_string_lossy());
    assert_eq!(media[2], root.join("media/Amnesia.mp3").to_string_lossy());
    assert_eq!(
        media[3],
        gone.to_string_lossy(),
        "a file that was not there keeps its path"
    );
    assert_eq!(media[4], media[0]);
    assert_eq!(std::fs::read(&media[0]).unwrap(), b"first film");
    assert_eq!(std::fs::read(&media[1]).unwrap(), b"second film, same name");
    assert_eq!(std::fs::read(&media[2]).unwrap().len(), 3 * CHUNK + 17);
    let font = opened["fonts"][0]["path"].as_str().unwrap();
    assert_eq!(font, root.join("fonts/Face.ttf").to_string_lossy());
    assert_eq!(std::fs::read(font).unwrap(), b"glyphs");

    // Opened a second time, it is a second project beside the first.
    let again = import(&out, &projects, &mut quiet(), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(again.name, "Marty (2)");
    assert_eq!(PathBuf::from(&again.path), projects.join("Marty (2)"));
}

#[test]
fn a_project_packed_without_its_media_keeps_their_paths() {
    let here = tempfile::tempdir().unwrap();
    let clip = here.path().join("clip.mp4");
    write(&clip, b"film");
    let document = document("Light", &[&clip.to_string_lossy()], &[]);
    let out = here.path().join("Light.zip");
    let packed = export(
        &document,
        &out,
        false,
        &mut quiet(),
        &AtomicBool::new(false),
    )
    .unwrap()
    .unwrap();
    assert_eq!(packed, Packed::default());
    assert_eq!(names(&out), vec!["concat.json"]);
    let info = import(&out, here.path(), &mut quiet(), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let opened = projects::read_document(&info.path).unwrap().unwrap();
    assert_eq!(opened["media"][0]["path"], clip.to_string_lossy().as_ref());
}

#[test]
fn a_stopped_export_or_import_leaves_nothing_behind() {
    let here = tempfile::tempdir().unwrap();
    let clip = here.path().join("clip.mp4");
    write(&clip, &vec![1u8; 4 * CHUNK]);
    let document = document("Stop", &[&clip.to_string_lossy()], &[]);
    let out = here.path().join("Stop.zip");

    let stop = AtomicBool::new(false);
    let stopped = export(
        &document,
        &out,
        true,
        &mut |done, _| {
            if done >= CHUNK as u64 {
                stop.store(true, Ordering::Relaxed);
            }
        },
        &stop,
    )
    .unwrap();
    assert!(stopped.is_none());
    assert!(!out.exists() && !partial_path(&out).exists());

    export(&document, &out, true, &mut quiet(), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let projects = here.path().join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    let stop = AtomicBool::new(false);
    let stopped = import(
        &out,
        &projects,
        &mut |done, _| {
            if done >= CHUNK as u64 {
                stop.store(true, Ordering::Relaxed);
            }
        },
        &stop,
    )
    .unwrap();
    assert!(stopped.is_none());
    assert_eq!(std::fs::read_dir(&projects).unwrap().count(), 0);
}

/// A ZIP written by hand, `entries` its names and contents in order.
fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
    let mut archive = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, bytes) in entries {
        archive
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        archive.write_all(bytes).unwrap();
    }
    archive.finish().unwrap();
}

#[test]
fn a_project_folder_zipped_by_hand_opens_without_its_cache() {
    let here = tempfile::tempdir().unwrap();
    let text = serde_json::to_vec(&document("By hand", &["/old/phone/clip.mp4"], &[])).unwrap();
    let out = here.path().join("hand.zip");
    zip_of(
        &out,
        &[
            ("By hand/concat.json", &text),
            ("By hand/concat.opened.json", b"{}"),
            ("By hand/cache/abc.peaks", b"peaks"),
            ("By hand/notes.txt", b"kept"),
            ("Other/deeper/concat.json", b"not this one"),
        ],
    );
    let info = import(&out, here.path(), &mut quiet(), &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let root = PathBuf::from(&info.path);
    assert_eq!(info.name, "By hand");
    assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"kept");
    assert!(!root.join("cache").exists());
    assert!(!root.join("concat.opened.json").exists());
    let opened = projects::read_document(&info.path).unwrap().unwrap();
    assert_eq!(opened["media"][0]["path"], "/old/phone/clip.mp4");
}

#[test]
fn an_archive_that_would_write_outside_its_folder_is_refused_whole() {
    let here = tempfile::tempdir().unwrap();
    let text = serde_json::to_vec(&document("Evil", &[], &[])).unwrap();
    let projects = here.path().join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    for bad in [
        "../escaped.txt",
        "media/../../escaped.txt",
        "media//x",
        "C:/x",
    ] {
        let out = here.path().join("evil.zip");
        zip_of(
            &out,
            &[
                ("concat.json", &text),
                ("media/fine.mp4", b"ok"),
                (bad, b"no"),
            ],
        );
        let refused = import(&out, &projects, &mut quiet(), &AtomicBool::new(false));
        assert!(refused.is_err(), "{bad} was let through");
        assert_eq!(
            std::fs::read_dir(&projects).unwrap().count(),
            0,
            "{bad}: something was written"
        );
        assert!(!here.path().join("escaped.txt").exists());
    }
    let none = here.path().join("none.zip");
    zip_of(&none, &[("readme.txt", b"no project here")]);
    assert!(import(&none, &projects, &mut quiet(), &AtomicBool::new(false)).is_err());
    assert!(
        import(
            &here.path().join("missing.zip"),
            &projects,
            &mut quiet(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn names_in_the_archive_are_cleaned_and_numbered() {
    let mut taken = HashSet::new();
    assert_eq!(unique_name(&mut taken, MEDIA, "a.mp4"), "media/a.mp4");
    assert_eq!(unique_name(&mut taken, MEDIA, "A.MP4"), "media/A (2).MP4");
    assert_eq!(unique_name(&mut taken, MEDIA, "a.mp4"), "media/a (3).mp4");
    assert_eq!(
        unique_name(&mut taken, MEDIA, "what?.mov"),
        "media/what-.mov"
    );
    assert_eq!(unique_name(&mut taken, MEDIA, "noext"), "media/noext");
    assert_eq!(unique_name(&mut taken, FONTS, "a.mp4"), "fonts/a.mp4");
    assert_eq!(
        safe_relative("media/a b.mp4"),
        Some(PathBuf::from("media/a b.mp4"))
    );
    assert_eq!(safe_relative("/etc/passwd"), None);
    assert_eq!(safe_relative("a\\b"), None);
    assert_eq!(safe_relative(""), None);
}
