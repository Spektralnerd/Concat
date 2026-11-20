// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Fork: the fast preview. With it on, every video larger than a quarter
//! of full HD gets a small working copy - a proxy, written once into the
//! project's cache on the proxy lane (`concat_host::proxy`) - and the
//! monitor shows the copy at rest, scrubbed and playing alike: the same
//! frames at the same moments, so a cut lands on the frame it would on
//! the original, decoded several times quicker. The export never reads a
//! copy. While copies are written, a line at the top says how far along.

use std::path::Path;
use std::time::Duration;

use super::*;

/// The line at the top while copies are written: how far the one under
/// way is and the time left first, the file last - a long name is cut off
/// at the screen's edge, and the numbers are what the line is for.
fn progress_line(progress: &concat_host::proxy::Progress) -> String {
    let Some(source) = progress.source.as_deref() else {
        return t("fast.queued");
    };
    let mut line = if progress.fraction > 0.0 {
        let percent = (progress.fraction * 100.0).floor() as u32;
        let mut line = tf("fast.writing", &[&percent]);
        // A guess only once there is something to guess from.
        if progress.fraction > 0.02 {
            let left = progress.elapsed * (1.0 - progress.fraction) / progress.fraction;
            line.push_str(" · ");
            line.push_str(&crate::format::eta(left));
        }
        line
    } else {
        t("fast.writingUnknown")
    };
    if progress.waiting > 0 {
        line.push_str(" · ");
        line.push_str(&tf("fast.waiting", &[&progress.waiting]));
    }
    let name = Path::new(source).file_name().map_or_else(
        || source.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    line.push_str(" · ");
    line.push_str(&name);
    line
}

impl Studio {
    /// The fast preview on or off, from now on and in the next run too.
    /// On, the media already in the project get their copies straight
    /// away - the files seen to under the higher line are asked again;
    /// either way the monitor's frame is drawn again from what it reads
    /// now.
    pub(crate) fn fast_preview_set(&mut self, on: bool) {
        log::info!("settings: fast preview {}", if on { "on" } else { "off" });
        self.prefs.fast_preview = on;
        self.prefs.save(&self.host.dirs);
        if on {
            self.proxied.clear();
            self.request_media_art();
        }
        self.request_preview();
    }

    /// Keeps the line at the top in step with the copies being written,
    /// once a second until the last is in - and then draws the monitor's
    /// frame again, from the copy.
    pub(super) fn watch_proxies(&mut self) {
        if self.proxy_watch.running() {
            return;
        }
        self.proxy_watch.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                crate::host::Shell::with(|shell, app| {
                    let progress = concat_host::proxy::progress();
                    let line = progress.as_ref().map(progress_line).unwrap_or_default();
                    app.global::<Editor>().set_proxy_status(line.into());
                    if progress.is_none() {
                        let mut studio = shell.studio.borrow_mut();
                        studio.proxy_watch.stop();
                        studio.request_preview();
                    }
                });
            },
        );
    }
}
