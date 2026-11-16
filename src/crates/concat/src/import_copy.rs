// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Spektralnerd (fork of jub0t/Concat)

//! Fork: the card a phone shows while it copies picked files into the
//! app, and what it says when one cannot be copied.
//!
//! A phone's picker hands over documents, not paths, so Android's crate
//! copies each picked file into the app's own folder before the engine
//! imports it (ConcatFiles.java). A film of several gigabytes takes
//! minutes, and the window said nothing the whole time; a phone without
//! the room for it dropped the copy without a word. The phone's crate now
//! reports the copy here, from its own thread, and the window shows it on
//! a card at the top with a Cancel.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::host::Shell;
use crate::i18n::{t, tf};
use crate::ui::ImportCopyData;

/// How far the copy of one picked file has come, as the phone reports it.
#[derive(Clone, Debug)]
pub struct ImportCopy {
    /// The file's name, as the picker showed it.
    pub name: String,
    /// Which of the picked files this is, from 0.
    pub index: usize,
    /// How many files were picked.
    pub count: usize,
    /// Bytes copied so far.
    pub copied: u64,
    /// The file's size, when the phone's provider says.
    pub total: Option<u64>,
}

/// Why a picked file was not copied.
#[derive(Clone, Debug)]
pub enum ImportProblem {
    /// The file would not fit, or the phone filled up while copying it.
    NoSpace {
        /// The file's name.
        name: String,
        /// Its size in bytes.
        size: u64,
        /// The room there was, in bytes.
        free: u64,
    },
    /// Reading or writing failed for another reason.
    Failed {
        /// The file's name.
        name: String,
        /// What the phone said.
        reason: String,
    },
    /// The person stopped the copy.
    Cancelled,
}

/// What the card's Cancel calls: installed by the phone's crate.
pub type ImportCanceller = Box<dyn Fn() + Send + Sync>;

static CANCELLER: OnceLock<ImportCanceller> = OnceLock::new();

/// Installs how the window asks the phone to stop copying. Once; a second
/// call is ignored.
pub fn install_import_canceller(cancel: ImportCanceller) {
    let _ = CANCELLER.set(cancel);
}

/// The card's Cancel: the copy stops at its next chunk, and the phone then
/// reports the end the usual way. A project's ZIP being written or opened
/// on the card is stopped instead.
pub(crate) fn cancel() {
    if let Some(job) = JOB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        log::info!("project zip: stopped on its card");
        job.store(true, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    log::info!("import: copy cancelled on its card");
    if let Some(cancel) = CANCELLER.get() {
        cancel();
    }
}

/// The stop of the project ZIP the card shows, while it shows one; see
/// [`job_started`].
static JOB: Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = Mutex::new(None);

/// A project's ZIP is being written or opened, and `cancel` stops it: the
/// card's Cancel reaches it from now until [`job_finished`]. False when one
/// is under way already, which keeps the card and its Cancel.
pub(crate) fn job_started(cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> bool {
    let mut slot = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_some() {
        return false;
    }
    *slot = Some(cancel);
    true
}

/// Puts the card up for a project ZIP, or moves it on: `title` over `done`
/// bytes of `total`, with the time left once it can be told. From any
/// thread.
pub(crate) fn job_progress(title: String, done: u64, total: u64, started: Instant) {
    let progress = ImportCopy {
        name: String::new(),
        index: 0,
        count: 1,
        copied: done,
        total: Some(total),
    };
    let (_, detail, fraction) = card_text(&progress, started.elapsed().as_secs_f32());
    let _ = slint::invoke_from_event_loop(move || {
        Shell::with(|_shell, app| {
            app.set_import_copy(ImportCopyData {
                active: true,
                title: title.into(),
                detail: detail.into(),
                fraction,
            });
        });
    });
}

/// The card for a project ZIP over a step that has no bar to fill: `title`
/// over `detail`, the bar running. From any thread.
pub(crate) fn job_note(title: String, detail: String) {
    let _ = slint::invoke_from_event_loop(move || {
        Shell::with(|_shell, app| {
            app.set_import_copy(ImportCopyData {
                active: true,
                title: title.into(),
                detail: detail.into(),
                fraction: -1.0,
            });
        });
    });
}

/// Fork: whether a copy into the app or a project ZIP is under way: the
/// files it is writing are no project's yet, and a clean-up must wait.
pub(crate) fn busy() -> bool {
    JOB.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
        || STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
}

/// Takes the card down after a project ZIP, and lets the next one start.
/// From any thread.
pub(crate) fn job_finished() {
    *JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    let _ = slint::invoke_from_event_loop(|| {
        Shell::with(|_shell, app| app.set_import_copy(ImportCopyData::default()));
    });
}

/// When the file being copied started, by its index: what the time left
/// is reckoned from.
static STARTED: Mutex<Option<(usize, Instant)>> = Mutex::new(None);

/// Puts the card up, or moves it on. From any thread.
pub fn import_copy_progress(progress: ImportCopy) {
    let elapsed = {
        let mut slot = STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *slot {
            Some((index, at)) if index == progress.index => at.elapsed().as_secs_f32(),
            _ => {
                *slot = Some((progress.index, Instant::now()));
                0.0
            }
        }
    };
    let (title, detail, fraction) = card_text(&progress, elapsed);
    let _ = slint::invoke_from_event_loop(move || {
        Shell::with(|_shell, app| {
            app.set_import_copy(ImportCopyData {
                active: true,
                title: title.into(),
                detail: detail.into(),
                fraction,
            });
        });
    });
}

/// Takes the card down: the copy is over, however it ended. From any thread.
pub fn import_copy_finished() {
    *STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    let _ = slint::invoke_from_event_loop(|| {
        Shell::with(|_shell, app| app.set_import_copy(ImportCopyData::default()));
    });
}

/// Says why a picked file was not copied, on the window's notice. From any
/// thread.
pub fn import_copy_problem(problem: ImportProblem) {
    let (message, failed) = match &problem {
        ImportProblem::NoSpace { name, size, free } => (
            tf(
                "importCopy.noSpace",
                &[
                    name,
                    &crate::format::bytes(*size as f32),
                    &crate::format::bytes(*free as f32),
                ],
            ),
            true,
        ),
        ImportProblem::Failed { name, reason } => (tf("importCopy.failed", &[name, reason]), true),
        ImportProblem::Cancelled => (t("importCopy.cancelled"), false),
    };
    log::warn!("import: {problem:?}");
    crate::host::on_ui(move |studio, _app, _models| studio.notify(&message, failed));
}

/// The card's three lines: which file, how far, and the bar's fill (below
/// zero when the size is not known).
fn card_text(progress: &ImportCopy, elapsed: f32) -> (String, String, f32) {
    let title = if progress.count > 1 {
        tf(
            "importCopy.titleMany",
            &[&progress.name, &(progress.index + 1), &progress.count],
        )
    } else {
        tf("importCopy.title", &[&progress.name])
    };
    let Some(total) = progress.total.filter(|total| *total > 0) else {
        let detail = tf(
            "importCopy.detailUnknown",
            &[&crate::format::bytes(progress.copied as f32)],
        );
        return (title, detail, -1.0);
    };
    let fraction = (progress.copied as f64 / total as f64).clamp(0.0, 1.0) as f32;
    let percent = (fraction * 100.0).floor() as u32;
    let mut detail = tf(
        "importCopy.detail",
        &[
            &crate::format::bytes(progress.copied as f32),
            &crate::format::bytes(total as f32),
            &percent,
        ],
    );
    // The time left only once there is a rate worth reckoning from.
    if elapsed >= 3.0 && progress.copied > 0 && progress.copied < total {
        let rate = progress.copied as f32 / elapsed;
        let left = (total - progress.copied) as f32 / rate;
        detail = format!("{detail} · {}", crate::format::eta(left));
    }
    (title, detail, fraction)
}
