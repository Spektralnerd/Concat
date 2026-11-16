// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: the copies a phone keeps of the files it was handed, and which of
//! them nothing uses any more.
//!
//! A phone's picker hands over documents, not paths, so every file picked
//! is copied into the app's `Imported` folder first (ConcatFiles.java),
//! and a project names the copy. A file taken out of a project leaves its
//! copy behind - Undo needs it - and so do a subtitle file read once, a
//! sound kept in the library, which has a copy of its own, and a ZIP
//! opened as a project. Over a few projects that is gigabytes of a phone
//! nothing will ever read again.
//!
//! A copy is in use while anything Concat keeps names it: a project's
//! document anywhere under the app's own folders or in the recents list,
//! the open project as it stands in memory - its file may lag an autosave
//! behind - and Concat's settings and libraries. Everything else in the
//! folder, older than a grace, is left over. A name is looked for loosely,
//! `Imported/<name>` anywhere in a string with case and the slash's
//! direction set aside, because a copy kept by mistake costs room and one
//! deleted by mistake costs a film.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// The folder a phone copies picked files into, under its `home`: the
/// app's external files folder on Android, its container on an iPhone.
pub fn folder(home: &Path) -> PathBuf {
    if cfg!(target_os = "ios") {
        home.join("Documents").join("Imported")
    } else {
        home.join("Imported")
    }
}

/// What is looked through for a copy's name.
#[derive(Clone, Debug, Default)]
pub struct Search {
    /// The copies' folder. Only what lies in it, not below, is a copy.
    pub imported: PathBuf,
    /// Folders read all the way down: every `.json` and `.toml` in them.
    /// The copies' folder is passed over wherever it lies, and so is
    /// everything in `skip`.
    pub roots: Vec<PathBuf>,
    /// Folders passed over: the logs, which name every file ever picked,
    /// and the models.
    pub skip: Vec<PathBuf>,
    /// Documents read besides, wherever they are: the projects of the
    /// recents list.
    pub files: Vec<PathBuf>,
    /// Documents left unread, because what they say stands in `open` as it
    /// is now: the open project's own file.
    pub stale: Vec<PathBuf>,
    /// Documents in memory: the open project.
    pub open: Vec<Value>,
    /// How old a copy must be to count as left over. A younger one may be
    /// on its way into a project still.
    pub grace: Duration,
}

/// The copies nothing names, in name order, and their bytes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Leftovers {
    /// The copies, by their paths.
    pub files: Vec<PathBuf>,
    /// Their bytes together: what deleting them frees.
    pub bytes: u64,
}

/// What [`delete`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Deleted {
    /// Copies deleted.
    pub files: usize,
    /// Their bytes.
    pub bytes: u64,
    /// The copies that would not go, each with why.
    pub failed: Vec<String>,
}

/// No settings file or project is this big, and a film someone named
/// `.json` is not a document.
const LARGEST: u64 = 64 * 1024 * 1024;

/// Deeper than this, a folder is not one Concat made.
const DEEPEST: usize = 16;

/// What comes before a copy's name, in lowercase with forward slashes.
const MARK: &str = "imported/";

/// The copies in the folder that nothing in `search` names.
pub fn leftovers(search: &Search) -> Leftovers {
    let mut copies = copies(search);
    if !copies.is_empty() {
        let mut named = Named {
            copies: &mut copies,
        };
        for document in &search.open {
            named.value(document);
        }
        for file in &search.files {
            if !search.stale.iter().any(|stale| same(stale, file)) {
                named.file(file);
            }
        }
        let imported = canonical(&search.imported);
        let skip: Vec<PathBuf> = search.skip.iter().map(|path| canonical(path)).collect();
        for root in &search.roots {
            walk(root, &imported, &skip, search, &mut named, 0);
        }
    }
    let mut files: Vec<(PathBuf, u64)> = copies.into_values().collect();
    files.sort();
    Leftovers {
        bytes: files.iter().map(|(_, size)| size).sum(),
        files: files.into_iter().map(|(path, _)| path).collect(),
    }
}

/// Deletes the copies of `confirmed` that are left over still: the search
/// is made again first, so a copy a project took up since it was asked
/// about stays, and nothing outside the copies' folder is ever touched.
pub fn delete(search: &Search, confirmed: &[PathBuf]) -> Deleted {
    let mut deleted = Deleted::default();
    for path in leftovers(search).files {
        if !confirmed.contains(&path) {
            continue;
        }
        let size = std::fs::symlink_metadata(&path).map_or(0, |meta| meta.len());
        match std::fs::remove_file(&path) {
            Ok(()) => {
                deleted.files += 1;
                deleted.bytes += size;
            }
            Err(error) => deleted.failed.push(format!("{}: {error}", path.display())),
        }
    }
    deleted
}

/// The copies old enough to count, by their name in lowercase: plain files
/// right in the folder, never a folder or a link.
fn copies(search: &Search) -> HashMap<String, (PathBuf, u64)> {
    let mut copies = HashMap::new();
    let Ok(entries) = std::fs::read_dir(&search.imported) else {
        return copies;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        // The entry's own kind: a link is not followed.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        // A copy whose age cannot be told is kept, like a young one.
        let old = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= search.grace);
        if !old {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        copies.insert(name, (entry.path(), meta.len()));
    }
    copies
}

/// Reads the documents under `folder`, down to [`DEEPEST`].
fn walk(
    folder: &Path,
    imported: &Path,
    skip: &[PathBuf],
    search: &Search,
    named: &mut Named,
    depth: usize,
) {
    if depth > DEEPEST || named.done() {
        return;
    }
    let here = canonical(folder);
    if here == imported || skip.contains(&here) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, imported, skip, search, named, depth + 1);
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let document = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("toml")
            });
        if document && !search.stale.iter().any(|stale| same(stale, &path)) {
            named.file(&path);
        }
    }
}

/// The copies not yet found named, taken off as they are.
struct Named<'a> {
    copies: &'a mut HashMap<String, (PathBuf, u64)>,
}

impl Named<'_> {
    fn done(&self) -> bool {
        self.copies.is_empty()
    }

    /// A document on disk: its strings, if it is JSON; its text, if it is
    /// anything else or JSON that does not parse.
    fn file(&mut self, path: &Path) {
        if self.done() {
            return;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if !meta.is_file() || meta.len() > LARGEST {
            return;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if json && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            self.value(&value);
            return;
        }
        self.text(&String::from_utf8_lossy(&bytes));
    }

    fn value(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.text(text),
            Value::Array(items) => {
                for item in items {
                    self.value(item);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    self.text(key);
                    self.value(item);
                }
            }
            _ => {}
        }
    }

    /// Takes every copy `text` names off the list.
    fn text(&mut self, text: &str) {
        if self.done() {
            return;
        }
        let lower = text.replace('\\', "/").to_lowercase();
        let mut from = 0;
        while let Some(at) = lower[from..].find(MARK) {
            from += at + MARK.len();
            let rest = &lower[from..];
            self.copies.retain(|name, _| !names(rest, name));
        }
    }
}

/// Whether `rest`, what follows `imported/` in a string, names the copy
/// `name`: it starts with the name, and what comes after it, if anything,
/// is no more of a file's name - the end of the string, a quote, a comma.
fn names(rest: &str, name: &str) -> bool {
    rest.strip_prefix(name).is_some_and(|after| {
        after
            .chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || matches!(next, '.' | '-' | '_')))
    })
}

/// `path` with its links and dots resolved, or as it is when it cannot be.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same(a: &Path, b: &Path) -> bool {
    a == b || canonical(a) == canonical(b)
}

#[cfg(test)]
#[path = "imports_tests.rs"]
mod tests;
