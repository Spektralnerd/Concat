// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: the open project saved as one ZIP, and a ZIP opened as a new
//! project (`concat_host::project_zip`), on the import card with its bar
//! and its Cancel.
//!
//! A phone packs into the projects' own folder and then hands the file to
//! Download/Concat, where it outlives the app; a desk asks where it goes.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use concat_host::project_zip::Media;

use super::*;

/// How often the card is moved on: a gigabyte goes by in a thousand
/// chunks, and the window need not hear of every one.
const CARD_EVERY: Duration = Duration::from_millis(200);

/// Fork: which files a project's ZIP takes along, as its button says.
#[derive(Clone, Copy)]
enum Pick {
    All,
    Own,
    None,
}

/// A progress report that says so at most every [`CARD_EVERY`], and at the
/// end.
fn card(title: String) -> impl FnMut(u64, u64) {
    let started = Instant::now();
    let mut said: Option<Instant> = None;
    move |done, total| {
        let due = said.is_none_or(|at| at.elapsed() >= CARD_EVERY) || done >= total;
        if due {
            said = Some(Instant::now());
            crate::import_copy::job_progress(title.clone(), done, total, started);
        }
    }
}

impl Studio {
    /// Saves the open project as a ZIP: with every media file and font it
    /// uses when `with_media`, else its document alone. On a phone it ends
    /// in Download/Concat; a desk asks where.
    pub fn project_zip_export(&mut self, with_media: bool) {
        self.project_zip_export_as(if with_media { Pick::All } else { Pick::None });
    }

    /// Fork: the open project as a ZIP with the files in its own folder -
    /// its smooth slow motion copies, a template's music - and every other
    /// file named by its path: a film used where it lies is not carried
    /// again.
    pub fn project_zip_export_own(&mut self) {
        self.project_zip_export_as(Pick::Own);
    }

    fn project_zip_export_as(&mut self, pick: Pick) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let document = session.document();
        let root = PathBuf::from(session.path());
        let media = match pick {
            Pick::All => Media::All,
            Pick::Own => Media::Own(root.clone()),
            Pick::None => Media::None,
        };
        let name = self.project_name.clone();
        let file_name = format!("{}.zip", concat_host::projects::folder_name(&name));
        if cfg!(any(target_os = "android", target_os = "ios")) {
            // Beside the project folders, then handed to the phone's files.
            let folder = root.parent().map(Path::to_path_buf).unwrap_or(root);
            let out = folder.join(&file_name);
            self.pack(document, out, media, name, true);
            return;
        }
        let title = t("projectZip.dialogTitle");
        let family = t("projectZip.family");
        let start = root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(home_folder("Documents")));
        crate::host::after_dialog(
            move || {
                let chosen = crate::platform::save_file(
                    &title,
                    &start,
                    &file_name,
                    (family.as_str(), &["zip"]),
                )?;
                Some(
                    if chosen
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
                    {
                        chosen
                    } else {
                        chosen.with_extension("zip")
                    },
                )
            },
            move |studio, _, _, out| studio.pack(document, out, media, name, false),
        );
    }

    /// Packs `document` into `out` on a worker, on the card; `hand_over`
    /// gives the finished file to the phone's Download/Concat.
    fn pack(
        &mut self,
        document: serde_json::Value,
        out: PathBuf,
        media: Media,
        name: String,
        hand_over: bool,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        if !crate::import_copy::job_started(Arc::clone(&cancel)) {
            self.notify(&t("projectZip.busy"), true);
            return;
        }
        log::info!(
            "project zip: packing {name} {}",
            match media {
                Media::All => "with its media",
                Media::Own(_) => "with the files in its folder",
                Media::None => "without its media",
            }
        );
        let mut progress = card(tf("projectZip.packing", &[&name]));
        progress(0, 0);
        let began = Instant::now();
        crate::host::spawn(
            move || {
                let packed = concat_host::project_zip::export(
                    &document,
                    &out,
                    &media,
                    &mut progress,
                    &cancel,
                );
                (out, packed)
            },
            move |studio, _, _, (out, packed)| {
                // A phone's ZIP keeps the card while it moves to Download.
                if !(hand_over && matches!(packed, Ok(Some(_)))) {
                    crate::import_copy::job_finished();
                }
                match packed {
                    Ok(Some(packed)) => {
                        log::info!(
                            "project zip: {} files, {} bytes packed in {}{}{}",
                            packed.files,
                            packed.bytes,
                            concat_log::span(began.elapsed()),
                            if packed.kept == 0 {
                                String::new()
                            } else {
                                format!("; {} from outside its folder named by path", packed.kept)
                            },
                            if packed.missing.is_empty() {
                                String::new()
                            } else {
                                format!("; {} not there, kept by path", packed.missing.len())
                            }
                        );
                        let missing = packed.missing.len();
                        let kept = packed.kept;
                        let say = move |studio: &mut Studio, place: &str| {
                            let said = if missing > 0 {
                                tf("projectZip.savedMissing", &[&place, &missing])
                            } else if kept > 0 {
                                tf("projectZip.savedKept", &[&place, &kept])
                            } else {
                                tf("projectZip.saved", &[&place])
                            };
                            studio.notify(&said, false);
                        };
                        if !hand_over {
                            say(studio, &out.to_string_lossy());
                            return;
                        }
                        let size = std::fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0);
                        crate::import_copy::job_note(
                            t("projectZip.handing"),
                            crate::format::bytes(size as f32),
                        );
                        let kept = out.clone();
                        let handed = crate::platform::publish_document(
                            out.clone(),
                            "application/zip",
                            move |result| {
                                crate::import_copy::job_finished();
                                crate::host::on_ui(move |studio, _, _| match result {
                                    Ok(place) => say(studio, &place),
                                    Err(error) => {
                                        log::warn!(
                                            "project zip: wanted it handed to the phone's files and it was not: {error}; it stays beside the projects"
                                        );
                                        studio.notify(
                                            &tf("projectZip.notHanded", &[&kept.display(), &error]),
                                            true,
                                        );
                                    }
                                });
                            },
                        );
                        if !handed {
                            crate::import_copy::job_finished();
                            say(studio, &out.to_string_lossy());
                        }
                    }
                    Ok(None) => studio.notify(&t("projectZip.stopped"), false),
                    Err(error) => {
                        log::warn!("project zip: packing failed: {error}");
                        studio.notify(&tf("projectZip.failed", &[&error]), true);
                    }
                }
            },
        );
    }

    /// Opens the ZIP at `archive` as a new project in `location`, and opens
    /// that. A phone's picker copied the ZIP into the app first; that copy
    /// goes once it is unpacked.
    pub(crate) fn project_zip_import(&mut self, archive: PathBuf, location: String) {
        let cancel = Arc::new(AtomicBool::new(false));
        if !crate::import_copy::job_started(Arc::clone(&cancel)) {
            self.notify(&t("projectZip.busy"), true);
            return;
        }
        let label = archive
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        log::info!("project zip: opening {label}");
        let mut progress = card(tf("projectZip.unpacking", &[&label]));
        progress(0, 0);
        crate::host::spawn(
            move || {
                let opened = concat_host::project_zip::import(
                    &archive,
                    Path::new(&location),
                    &mut progress,
                    &cancel,
                );
                // The phone's copy of what was picked, not the person's own
                // file: a desk's picker hands over the file itself.
                if cfg!(any(target_os = "android", target_os = "ios"))
                    && archive
                        .components()
                        .any(|part| part.as_os_str() == "Imported")
                {
                    let _ = std::fs::remove_file(&archive);
                }
                opened
            },
            move |studio, _, _, opened| {
                crate::import_copy::job_finished();
                match opened {
                    Ok(Some(info)) => {
                        log::info!("project zip: opened as {}", info.name);
                        let name = info.name.clone();
                        match studio.open_project_at(&info.path) {
                            Ok(()) => studio.notify(&tf("projectZip.imported", &[&name]), false),
                            Err(error) => studio.notify(&error, true),
                        }
                    }
                    Ok(None) => studio.notify(&t("projectZip.stopped"), false),
                    Err(error) => {
                        log::warn!("project zip: opening failed: {error}");
                        studio.notify(&tf("projectZip.importFailed", &[&error]), true);
                    }
                }
            },
        );
    }
}
