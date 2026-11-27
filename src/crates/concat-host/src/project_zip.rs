// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: a project as one ZIP file - its document and every file it uses -
//! to keep beside the phone or carry to another one, and back again.
//!
//! A phone keeps a project, and the media picked into it, in the app's own
//! folder, which goes with the app when it is uninstalled. Packed into one
//! file in Download, it outlives that.
//!
//! The archive holds `concat.json`, the document, with its media and fonts
//! pointed at `media/...` and `fonts/...` inside the archive, and those
//! files, stored as they are: a film is compressed already, and storing it
//! costs no more than copying it. Opening one makes a fresh project folder
//! beside the others, the files in its `media/` and `fonts/` and the
//! document pointing at them there. An archive packed without its media
//! keeps the paths the files had; a project folder zipped by hand opens as
//! well, its document wherever in the archive it is.
//!
//! Between the two, [`Media::Own`] packs only what lies in the project's
//! own folder - its smooth slow motion copies, the music a template's ZIP
//! brought in - and names every other file by its path: a film used where
//! it lies stays there, and the ZIP does not carry it again.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::projects::{self, ProjectInfo};

/// The document's name in the archive.
pub const DOCUMENT: &str = "concat.json";
/// The document's name in a project folder from before the app's rename,
/// as a folder zipped by hand may hold it.
const LEGACY_DOCUMENT: &str = "wolfcut.json";
const MEDIA: &str = "media";
const FONTS: &str = "fonts";
/// How much is read and written at a time, and how often progress is said.
const CHUNK: usize = 1 << 20;

/// Fork: which of the files a document names are packed beside it.
#[derive(Clone, Debug, PartialEq)]
pub enum Media {
    /// None of them: each keeps its path.
    None,
    /// Those in the project's own folder, `root`, and no others: its smooth
    /// slow motion copies, the music a template's ZIP brought in. A file
    /// from anywhere else - a film used where it lies - keeps its path.
    Own(PathBuf),
    /// Every media file and font.
    All,
}

impl Media {
    /// Whether the file at `path` goes into the archive.
    fn packs(&self, path: &Path) -> bool {
        match self {
            Media::None => false,
            Media::Own(root) => within(path, root),
            Media::All => true,
        }
    }
}

/// Whether `path` lies in the folder `root`: as written, or once both are
/// resolved, since a phone's storage answers to more than one path.
fn within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || matches!(
            (std::fs::canonicalize(path), std::fs::canonicalize(root)),
            (Ok(path), Ok(root)) if path.starts_with(&root)
        )
}

/// What an export packed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Packed {
    /// Files packed beside the document.
    pub files: usize,
    /// Their bytes.
    pub bytes: u64,
    /// The media and fonts the document names that were not there to pack;
    /// their paths are kept as they were.
    pub missing: Vec<String>,
    /// Fork: the files left out on purpose and named by their path - those
    /// outside the project's folder, for [`Media::Own`].
    pub kept: usize,
}

/// Packs `document` - a project's, as it would be saved - into a ZIP at
/// `out`, with the media files and fonts it names that `media` takes.
/// `progress` is told the bytes packed and the bytes in all as it goes.
/// `cancel` stops it between two chunks; None then comes back, and nothing
/// is left at `out`, which is written in full or not at all.
pub fn export(
    document: &Value,
    out: &Path,
    media: &Media,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Option<Packed>, String> {
    let mut document = document.clone();
    let mut entries: Vec<(PathBuf, String, u64)> = Vec::new();
    let mut missing = Vec::new();
    let mut kept: HashSet<String> = HashSet::new();
    if *media != Media::None {
        // Names taken in the archive, lowercase: a phone's SD card and
        // Windows tell no case apart, and two files would be one there.
        let mut taken: HashSet<String> = HashSet::new();
        let mut packed: HashMap<String, String> = HashMap::new();
        for (section, folder) in [("media", MEDIA), ("fonts", FONTS)] {
            let Some(items) = document.get_mut(section).and_then(Value::as_array_mut) else {
                continue;
            };
            for item in items {
                if item.get("placeholder").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(path) = item.get("path").and_then(Value::as_str).map(str::to_owned) else {
                    continue;
                };
                if path.is_empty() {
                    continue;
                }
                // One file used twice is packed once.
                if let Some(name) = packed.get(&path) {
                    item["path"] = Value::String(name.clone());
                    continue;
                }
                let source = PathBuf::from(&path);
                if !media.packs(&source) {
                    kept.insert(path);
                    continue;
                }
                let size = match std::fs::metadata(&source) {
                    Ok(meta) if meta.is_file() => meta.len(),
                    _ => {
                        missing.push(path);
                        continue;
                    }
                };
                let file_name = source
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let name = unique_name(&mut taken, folder, &file_name);
                packed.insert(path, name.clone());
                item["path"] = Value::String(name.clone());
                entries.push((source, name, size));
            }
        }
    }
    let total: u64 = entries.iter().map(|(_, _, size)| size).sum();
    let partial = partial_path(out);
    let written = write_archive(&partial, &document, &entries, total, progress, cancel);
    match written {
        Ok(true) => {
            std::fs::rename(&partial, out).map_err(|error| {
                let _ = std::fs::remove_file(&partial);
                format!("could not finish {}: {error}", out.display())
            })?;
            Ok(Some(Packed {
                files: entries.len(),
                bytes: total,
                missing,
                kept: kept.len(),
            }))
        }
        Ok(false) => {
            let _ = std::fs::remove_file(&partial);
            Ok(None)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// [`export`] for a project folder that is not open: its document as it
/// was last saved.
pub fn export_folder(
    project: &Path,
    out: &Path,
    media: &Media,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Option<Packed>, String> {
    let document = projects::read_document(&project.to_string_lossy())?
        .ok_or_else(|| format!("{} holds no project yet", project.display()))?;
    export(&document, out, media, progress, cancel)
}

/// Where an export is written before it is whole: beside `out`.
fn partial_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    out.with_file_name(name)
}

/// A name in `folder` of the archive for a file called `file_name`, kept
/// as it is but for what a file system would refuse, and numbered when
/// it is taken: "song (2).mp3".
fn unique_name(taken: &mut HashSet<String>, folder: &str, file_name: &str) -> String {
    let clean = projects::folder_name(file_name);
    let (stem, extension) = match clean.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem.to_owned(), format!(".{extension}")),
        _ => (clean.clone(), String::new()),
    };
    let mut number = 1;
    loop {
        let candidate = if number == 1 {
            format!("{folder}/{stem}{extension}")
        } else {
            format!("{folder}/{stem} ({number}){extension}")
        };
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
        number += 1;
    }
}

/// Writes the archive, true when it is whole and false when `cancel` came
/// first.
fn write_archive(
    path: &Path,
    document: &Value,
    entries: &[(PathBuf, String, u64)],
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let file = File::create(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    let mut archive = zip::ZipWriter::new(BufWriter::with_capacity(CHUNK, file));
    let zipped =
        |error: zip::result::ZipError| format!("could not write {}: {error}", path.display());
    let failed = |error: std::io::Error| format!("could not write {}: {error}", path.display());
    let text = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("could not write the document: {error}"))?;
    archive
        .start_file(
            DOCUMENT,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(zipped)?;
    archive.write_all(&text).map_err(failed)?;
    progress(0, total);
    let mut done = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    for (source, name, size) in entries {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(*size >= u64::from(u32::MAX));
        archive.start_file(name.as_str(), options).map_err(zipped)?;
        let mut input = File::open(source)
            .map_err(|error| format!("could not read {}: {error}", source.display()))?;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| format!("could not read {}: {error}", source.display()))?;
            if read == 0 {
                break;
            }
            archive.write_all(&buffer[..read]).map_err(failed)?;
            done += read as u64;
            progress(done, total);
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
        }
    }
    let writer = archive.finish().map_err(zipped)?;
    let file = writer
        .into_inner()
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.sync_all().map_err(failed)?;
    Ok(true)
}

/// Opens the ZIP at `archive` as a new project in `location`: a folder of
/// its own, named for the project and numbered when that is taken, with
/// the archive's files in it and the document pointing at them. `progress`
/// is told the bytes unpacked and the bytes in all. `cancel` stops it
/// between two chunks; None then comes back and the folder is gone again.
/// An archive with no project in it, or one that would write outside the
/// folder, is refused before anything is written.
pub fn import(
    archive: &Path,
    location: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Option<ProjectInfo>, String> {
    let opened = File::open(archive)
        .map_err(|error| format!("could not read {}: {error}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(opened))
        .map_err(|error| format!("{} is not a ZIP file: {error}", archive.display()))?;
    let unreadable =
        |error: zip::result::ZipError| format!("could not read {}: {error}", archive.display());

    // The document: the one nearest the top, so a project folder zipped by
    // hand, its document one folder down, opens too.
    let manifest = zip
        .file_names()
        .filter_map(|name| name.ok().map(|name| name.into_owned()))
        .filter(|name| {
            let last = name.rsplit('/').next().unwrap_or(name);
            last == DOCUMENT || last == LEGACY_DOCUMENT
        })
        .min_by_key(|name| (name.matches('/').count(), name.len()))
        .ok_or_else(|| format!("{} holds no Concat project", archive.display()))?;
    let prefix = manifest
        .rsplit_once('/')
        .map(|(folder, _)| format!("{folder}/"))
        .unwrap_or_default();
    let mut text = String::new();
    zip.by_name(&manifest)
        .map_err(unreadable)?
        .read_to_string(&mut text)
        .map_err(|error| {
            format!(
                "could not read the project in {}: {error}",
                archive.display()
            )
        })?;
    let mut document: Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "the project in {} cannot be read: {error}",
            archive.display()
        )
    })?;

    // What is unpacked: everything under the document's folder but the
    // document itself, the copy of it as last opened, and the cache, which
    // names files by where they were. Every name is checked first.
    let mut plan: Vec<(usize, PathBuf, u64)> = Vec::new();
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(unreadable)?;
        let name = entry.name().map_err(unreadable)?.into_owned();
        if entry.is_dir() || name.ends_with('/') {
            continue;
        }
        let Some(relative) = name.strip_prefix(&prefix) else {
            continue;
        };
        if relative == DOCUMENT
            || relative == LEGACY_DOCUMENT
            || relative.ends_with(".opened.json")
            || relative.starts_with("cache/")
        {
            continue;
        }
        let Some(safe) = safe_relative(relative).filter(|_| entry.enclosed_name().is_some()) else {
            return Err(format!(
                "{} would write outside its project ({name}); nothing was unpacked",
                archive.display()
            ));
        };
        plan.push((index, safe, entry.size()));
    }
    let total: u64 = plan.iter().map(|(_, _, size)| size).sum();

    let wanted = document
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            archive
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Imported project".to_owned())
        });
    let (name, root) = free_folder(location, &wanted)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("could not create {}: {error}", root.display()))?;
    let unpacked = unpack(&mut zip, &plan, &root, total, progress, cancel);
    match unpacked {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_dir_all(&root);
            return Ok(None);
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&root);
            return Err(error);
        }
    }

    // The paths the archive's own files had in it are paths in the folder
    // now; any other path - an archive packed without its media - is kept.
    let unpacked: HashSet<&Path> = plan.iter().map(|(_, path, _)| path.as_path()).collect();
    for section in ["media", "fonts"] {
        if let Some(items) = document.get_mut(section).and_then(Value::as_array_mut) {
            for item in items {
                let Some(path) = item.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let Some(relative) = safe_relative(path) else {
                    continue;
                };
                if unpacked.contains(relative.as_path()) {
                    item["path"] =
                        Value::String(root.join(&relative).to_string_lossy().into_owned());
                }
            }
        }
    }
    document["name"] = Value::String(name);
    let folder = root.to_string_lossy().into_owned();
    match projects::save(&folder, &document).and_then(|_| projects::open(&folder)) {
        Ok(info) => Ok(Some(info)),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&root);
            Err(error)
        }
    }
}

/// Writes the planned entries under `root`, true when all are there and
/// false when `cancel` came first.
fn unpack(
    zip: &mut zip::ZipArchive<BufReader<File>>,
    plan: &[(usize, PathBuf, u64)],
    root: &Path,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<bool, String> {
    progress(0, total);
    let mut done = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    for (index, relative, _) in plan {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let mut entry = zip
            .by_index(*index)
            .map_err(|error| format!("could not read {}: {error}", relative.display()))?;
        let file = File::create(&target)
            .map_err(|error| format!("could not write {}: {error}", target.display()))?;
        let mut output = BufWriter::with_capacity(CHUNK, file);
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|error| format!("could not unpack {}: {error}", relative.display()))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("could not write {}: {error}", target.display()))?;
            done += read as u64;
            progress(done, total);
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
        }
        output
            .flush()
            .map_err(|error| format!("could not write {}: {error}", target.display()))?;
    }
    Ok(true)
}

/// `name` as a path inside a folder: relative, with no step out of it, no
/// drive and no empty part. None for anything else.
fn safe_relative(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\\') || name.contains(':') || name.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        out.push(part);
    }
    out.components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(out)
}

/// A folder in `location` for a project called `wanted`, and the name it
/// goes by: the name itself, or "name (2)" and on when a folder has it.
fn free_folder(location: &Path, wanted: &str) -> Result<(String, PathBuf), String> {
    for number in 1..10_000 {
        let name = if number == 1 {
            wanted.to_owned()
        } else {
            format!("{wanted} ({number})")
        };
        let root = location.join(projects::folder_name(&name));
        if !root.exists() {
            return Ok((name, root));
        }
    }
    Err(format!(
        "no free folder for {wanted} in {}",
        location.display()
    ))
}

#[cfg(test)]
#[path = "project_zip_tests.rs"]
mod tests;
