// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: the media sheet's "Free up space". The phone keeps a copy of
//! every file it was handed (`concat_host::imports`); the copies no project
//! uses any more are found on a worker, asked about with what they come to,
//! and deleted once the answer is yes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use concat_host::imports::{self, Search};

use super::*;

/// A copy younger than this may still be on its way into the open project:
/// copied, and waiting for its probe.
const GRACE: Duration = Duration::from_secs(120);

/// The folders under the app's data that hold no document naming a copy,
/// and are big: the models, and the updates.
const NOT_DOCUMENTS: [&str; 4] = ["cutout-models", "whisper-models", "tts-models", "updates"];

impl Studio {
    /// What is looked through, as things stand now: the app's own folders,
    /// where the projects and the libraries are, the recents' projects, and
    /// the open project as it is in memory. None where there is no home to
    /// keep copies under.
    fn imports_search(&self) -> Option<Search> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let dirs = &self.host.dirs;
        // A folder inside another is read with it, once: a phone's settings
        // and data are one folder, and without external storage its home
        // is that folder's parent.
        let mut candidates = vec![home.clone(), dirs.config.clone(), dirs.data.clone()];
        candidates.sort_by_key(|path| path.components().count());
        let mut roots: Vec<PathBuf> = Vec::new();
        for root in candidates {
            if !roots.iter().any(|kept| root.starts_with(kept)) {
                roots.push(root);
            }
        }
        let mut skip = vec![concat_host::logs::folder(dirs)];
        skip.extend(NOT_DOCUMENTS.iter().map(|name| dirs.data.join(name)));
        let files = concat_host::projects::list(&dirs.config)
            .into_iter()
            .map(|info| concat_host::projects::manifest_path(Path::new(&info.path)))
            .collect();
        let (open, stale) = match self.session.as_ref() {
            Some(session) => {
                let root = Path::new(session.path());
                (
                    vec![session.document()],
                    vec![root.join("concat.json"), root.join("wolfcut.json")],
                )
            }
            None => (Vec::new(), Vec::new()),
        };
        Some(Search {
            imported: imports::folder(&home),
            roots,
            skip,
            files,
            stale,
            open,
            grace: GRACE,
        })
    }

    /// "Free up space": the copies nothing uses are looked for on a worker,
    /// then asked about - how many, how big - before any is deleted.
    pub fn imports_clean_up(&mut self) {
        if crate::import_copy::busy() {
            self.notify(&t("cleanup.busy"), true);
            return;
        }
        let Some(search) = self.imports_search() else {
            self.notify(&t("cleanup.nothing"), false);
            return;
        };
        log::info!(
            "cleanup: looking for copies nothing uses in {}",
            search.imported.display()
        );
        let began = Instant::now();
        crate::host::spawn(
            move || imports::leftovers(&search),
            move |studio, _, _, found| {
                log::info!(
                    "cleanup: {} copies of {} bytes nothing uses, found in {}",
                    found.files.len(),
                    found.bytes,
                    concat_log::span(began.elapsed())
                );
                if found.files.is_empty() {
                    studio.notify(&t("cleanup.nothing"), false);
                    return;
                }
                studio.confirm = Some(Confirm::DeleteLeftovers {
                    files: found.files,
                    bytes: found.bytes,
                });
            },
        );
    }

    /// The question's yes: the copies go, on a worker. The search is made
    /// again first, so one a project took up meanwhile stays.
    pub(crate) fn imports_delete(&mut self, files: Vec<PathBuf>) {
        let Some(search) = self.imports_search() else {
            return;
        };
        crate::host::spawn(
            move || imports::delete(&search, &files),
            move |studio, _, _, deleted| {
                log::info!(
                    "cleanup: deleted {} copies, {} bytes freed",
                    deleted.files,
                    deleted.bytes
                );
                for failure in &deleted.failed {
                    log::warn!("cleanup: wanted a copy gone and it stayed: {failure}");
                }
                let freed = crate::format::bytes(deleted.bytes as f32);
                // Each key spelled out where it is used: scripts/locales.py
                // finds the strings the source asks for by reading it.
                let said = match deleted.files {
                    0 => t("cleanup.nothing"),
                    1 => tf("cleanup.doneOne", &[&freed]),
                    many => tf("cleanup.done", &[&many, &freed]),
                };
                studio.notify(&said, false);
                if !deleted.failed.is_empty() {
                    studio.notify(&tf("cleanup.failed", &[&deleted.failed.len()]), true);
                }
            },
        );
    }
}
