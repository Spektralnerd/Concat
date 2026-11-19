// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! The activity's entry point. android-activity calls `android_main` on
//! its own thread once the activity is up; Slint's backend takes the
//! activity from there, and the window runs exactly as it does anywhere
//! else.
//!
//! Two things are settled here before the window starts, because only the
//! activity knows them. The app's directories: an Android process has no
//! home directory, so the host's XDG bases are pointed at the app's own
//! files folder, and its external files folder stands in for the home the
//! project and export defaults hang off. And where words go: a process on
//! a phone has no terminal, so the log facade and everything the window
//! prints to stderr are forwarded to logcat under the `concat` tag, which
//! is where `adb logcat -s concat` reads a report from.

#[cfg(target_os = "android")]
mod activity {
    use std::io::BufRead;

    /// Names the app's directories for the host. Set once, before any other
    /// thread exists, which is what makes writing the environment sound.
    pub fn name_directories(app: &slint::android::AndroidApp) {
        let Some(internal) = app.internal_data_path() else {
            return;
        };
        // What the phone keeps for the app: settings, recents, models.
        // SAFETY: called from android_main before the window or any worker
        // thread starts, so no other thread reads the environment.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &internal);
            std::env::set_var("XDG_DATA_HOME", &internal);
            // Projects and exports land under the folder the user can reach
            // through the phone's file manager, falling back to the private
            // one when there is no external storage.
            let home = app.external_data_path().unwrap_or(internal);
            std::env::set_var("HOME", home);
        }
    }

    /// Routes the log facade, panics and stderr to logcat, and to a file.
    ///
    /// logcat is what a phone on a cable gives you, and nothing else is as
    /// good while there is a cable. A phone in somebody's hand has none, so
    /// the same lines are written into the app's own storage as well; the
    /// window's Settings is where they are found. Called after
    /// [`name_directories`], because that is what says where storage is.
    pub fn open_log() {
        // Info is logcat's floor, as it always was; CONCAT_LOG is the
        // ceiling over both sinks, so turning the file up to debug does not
        // also flood logcat, and turning everything down still quiets it.
        let logcat = android_logger::AndroidLogger::new(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("concat"),
        );
        concat::open_logging(Some(Box::new(logcat)));
        forward_stderr();
    }

    /// Everything written to stderr is read back off a pipe and logged a
    /// line at a time, so a stray `eprintln!` in a dependency reaches logcat
    /// without that dependency knowing about phones. The app's own lines do
    /// not come this way - they go through the facade, which on a phone
    /// deliberately leaves stderr alone so a line cannot come back round
    /// this pipe and log itself forever.
    fn forward_stderr() {
        use std::os::fd::FromRawFd;
        let mut ends = [0i32; 2];
        // SAFETY: plain libc calls on descriptors this function owns; the
        // read end is handed to exactly one File.
        let reader = unsafe {
            if libc::pipe(ends.as_mut_ptr()) != 0 {
                return;
            }
            if libc::dup2(ends[1], libc::STDERR_FILENO) < 0 {
                libc::close(ends[0]);
                libc::close(ends[1]);
                return;
            }
            libc::close(ends[1]);
            std::fs::File::from_raw_fd(ends[0])
        };
        std::thread::Builder::new()
            .name("stderr-to-logcat".into())
            .spawn(move || {
                for line in std::io::BufReader::new(reader)
                    .lines()
                    .map_while(Result::ok)
                {
                    log::warn!("stderr: {line}");
                }
            })
            .ok();
    }
}

/// The Java in java/, and the way to it.
///
/// The window is a NativeActivity with no Java of its own, and some of
/// what a phone does - a picker's answer, the media store - comes only
/// through Java; so the Java is a fragment compiled by build.rs into a dex
/// the binary carries, loaded here through an in-memory class loader
/// once, and reached through [`java::with_class`].
#[cfg(target_os = "android")]
mod java {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use jni::objects::{Global, JClass, JObject};
    use jni::{Env, JavaVM, jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// The classes build.rs compiled: app.concat.editor.ConcatFiles.
    const DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));
    const CLASS_NAME: &str = "app.concat.editor.ConcatFiles";

    /// The fragment class, loaded once and kept.
    static CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

    /// Runs `with` on this thread, attached to the activity's JavaVM, with
    /// the activity and the fragment class - loaded the first time.
    pub fn with_class<T>(
        app: &AndroidApp,
        with: impl FnOnce(
            &mut Env<'_>,
            &JObject<'_>,
            &Global<JClass<'static>>,
        ) -> jni::errors::Result<T>,
    ) -> jni::errors::Result<T> {
        // SAFETY: the pointer is the activity's JavaVM, live for the
        // process; `from_raw` also seeds `JavaVM::singleton`, which the
        // native callback reaches for.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        vm.attach_current_thread(|env| {
            // SAFETY: the activity pointer is a live reference the app
            // holds for as long as it runs; it is only read here.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            let class = match CLASS.get() {
                Some(class) => class,
                None => {
                    let loaded = load_class(env, &activity)?;
                    let _ = CLASS.set(loaded);
                    CLASS.get().expect("set just above")
                }
            };
            with(env, &activity, class)
        })
    }

    /// Loads the fragment class from the dex through the activity's own
    /// class loader, and registers `filesPicked` on it.
    fn load_class(
        env: &mut Env,
        activity: &JObject,
    ) -> jni::errors::Result<Global<JClass<'static>>> {
        let parent = env
            .call_method(
                activity,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        // SAFETY: DEX is 'static and the loader never writes to it.
        let buffer = unsafe { env.new_direct_byte_buffer(DEX.as_ptr().cast_mut(), DEX.len()) }?;
        let loader = env.new_object(
            jni_str!("dalvik/system/InMemoryDexClassLoader"),
            jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
            &[(&buffer).into(), (&parent).into()],
        )?;
        let name = env.new_string(CLASS_NAME)?;
        let class = env
            .call_method(
                &loader,
                jni_str!("loadClass"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                &[(&name).into()],
            )?
            .l()?;
        let class = JClass::cast_local(env, class)?;
        // SAFETY: each signature is the Java declaration's, and each
        // function below takes exactly what it names.
        let methods = unsafe {
            [
                jni::NativeMethod::from_raw_parts(
                    jni_str!("filesPicked"),
                    jni_str!("([Ljava/lang/String;)V"),
                    super::picker::files_picked as *mut c_void,
                ),
                // Fork: the copy's progress and its problems.
                jni::NativeMethod::from_raw_parts(
                    jni_str!("importProgress"),
                    jni_str!("([Ljava/lang/String;IIJJ)V"),
                    super::picker::import_progress as *mut c_void,
                ),
                jni::NativeMethod::from_raw_parts(
                    jni_str!("importProblem"),
                    jni_str!("([Ljava/lang/String;JJ)V"),
                    super::picker::import_problem as *mut c_void,
                ),
                // Fork: the folder picker's answer.
                jni::NativeMethod::from_raw_parts(
                    jni_str!("folderPicked"),
                    jni_str!("([Ljava/lang/String;)V"),
                    super::picker::folder_picked as *mut c_void,
                ),
            ]
        };
        // SAFETY: as above.
        unsafe { env.register_native_methods(&class, &methods) }?;
        env.new_global_ref(class)
    }
}

/// The system's document picker.
///
/// A picker's result comes back only through an activity's or a
/// fragment's onActivityResult, so the Java fragment is added to the
/// activity for the length of one pick and told - by the native method
/// registered on it - where to bring the answer. The picked files are
/// copied into the app's own storage by the Java, and their paths are
/// what comes back.
#[cfg(target_os = "android")]
mod picker {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use jni::objects::{JObjectArray, JString};
    use jni::{JavaVM, jni_sig, jni_str, sys};
    use slint::android::AndroidApp;

    type Picked = Box<dyn FnOnce(Vec<PathBuf>) + Send>;

    /// The pick in flight, waiting for Java to answer.
    static PENDING: Mutex<Option<Picked>> = Mutex::new(None);

    /// Fork: a folder picker's answer; see `concat::FolderPicker`.
    type FolderPicked = Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>;

    /// Fork: the folder pick in flight, waiting for Java to answer.
    static FOLDER_PENDING: Mutex<Option<FolderPicked>> = Mutex::new(None);

    /// Hands the window's crate a picker that runs on this activity.
    pub fn install(app: &AndroidApp) {
        // Fork: the import card's Cancel, which the copy in the Java hears
        // at its next chunk.
        let canceller = app.clone();
        concat::install_import_canceller(Box::new(move || {
            let asked = super::java::with_class(&canceller, |env, _activity, class| {
                env.call_static_method(class, jni_str!("cancelImport"), jni_sig!("()V"), &[])?;
                Ok(())
            });
            if let Err(error) = asked {
                log::warn!("import: asking the copy to stop failed: {error}; it runs to its end");
            }
        }));
        // Fork: the folder picker, for projects on an SD card or a USB
        // drive.
        let folders = app.clone();
        concat::install_folder_picker(Box::new(move |on_picked| {
            let previous = FOLDER_PENDING
                .lock()
                .map(|mut slot| slot.replace(on_picked))
                .ok()
                .flatten();
            if let Some(previous) = previous {
                previous(Ok(None));
            }
            let asked = super::java::with_class(&folders, |env, activity, class| {
                env.call_static_method(
                    class,
                    jni_str!("pickFolder"),
                    jni_sig!("(Landroid/app/Activity;)V"),
                    &[activity.into()],
                )?;
                Ok(())
            });
            if let Err(error) = asked {
                log::error!(
                    "project: showing the folder picker failed: {error}; nothing was picked"
                );
                if let Some(pending) = FOLDER_PENDING.lock().ok().and_then(|mut slot| slot.take()) {
                    pending(Ok(None));
                }
            }
        }));
        let app = app.clone();
        concat::install_file_picker(Box::new(move |kinds, on_picked| {
            let previous = PENDING
                .lock()
                .map(|mut slot| slot.replace(on_picked))
                .ok()
                .flatten();
            if let Some(previous) = previous {
                // A pick was already up; the one that asked first is told
                // it got nothing rather than left waiting for ever.
                previous(Vec::new());
            }
            if let Err(error) = pick(&app, &kinds) {
                log::error!(
                    "import: showing the document picker failed: {error}; nothing was picked"
                );
                if let Some(pending) = PENDING.lock().ok().and_then(|mut slot| slot.take()) {
                    pending(Vec::new());
                }
            }
        }));
    }

    fn pick(app: &AndroidApp, kinds: &concat::PickKinds) -> jni::errors::Result<()> {
        let types = mime_types(kinds).join(",");
        // Fork: media are used where they lie when Settings says so and the
        // phone lets them be read there. A subtitle, a font, a project's
        // ZIP is copied as ever: small, read once, and the ZIP's copy is
        // the one its import may delete.
        let in_place = matches!(kinds, concat::PickKinds::Media { .. }) && concat::files_in_place();
        super::java::with_class(app, |env, activity, class| {
            let types = env.new_string(&types)?;
            env.call_static_method(
                class,
                jni_str!("pick"),
                jni_sig!("(Landroid/app/Activity;Ljava/lang/String;Z)V"),
                &[activity.into(), (&types).into(), in_place.into()],
            )?;
            Ok(())
        })
    }

    /// The picker's MIME types for what is asked: media by its families,
    /// and none at all - every file - for anything else. A subtitle or a
    /// font is typed so variously by the phone's providers
    /// (application/x-subrip, text/plain, application/octet-stream...)
    /// that a list would hide the very file asked for; what is picked is
    /// read for what it is, and a wrong file says so.
    pub(super) fn mime_types(kinds: &concat::PickKinds) -> Vec<&'static str> {
        match kinds {
            concat::PickKinds::Media {
                video,
                audio,
                image,
            } => [
                (*video, "video/*"),
                (*audio, "audio/*"),
                (*image, "image/*"),
            ]
            .into_iter()
            .filter_map(|(on, kind)| on.then_some(kind))
            .collect(),
            concat::PickKinds::Files(_) => Vec::new(),
        }
    }

    /// `ConcatFiles.filesPicked`, on whichever thread the Java copied on.
    pub(super) unsafe extern "system" fn files_picked(
        _env: *mut sys::JNIEnv,
        _class: sys::jclass,
        paths: sys::jobjectArray,
    ) {
        let read = JavaVM::singleton().and_then(|vm| {
            vm.attach_current_thread(|env| {
                // SAFETY: `paths` is the argument Java handed this frame.
                let array = unsafe { JObjectArray::<JString>::from_raw(env, paths) };
                let mut out = Vec::new();
                for index in 0..array.len(env)? {
                    let item = array.get_element(env, index)?;
                    out.push(PathBuf::from(item.mutf8_chars(env)?.to_string()));
                }
                Ok::<_, jni::errors::Error>(out)
            })
        });
        let paths = match read {
            Ok(paths) => paths,
            Err(error) => {
                log::error!("import: reading the picked files failed: {error}; nothing was picked");
                Vec::new()
            }
        };
        // Fork: the copy is over, however it went; its card goes.
        concat::import_copy_finished();
        if let Some(pending) = PENDING.lock().ok().and_then(|mut slot| slot.take()) {
            pending(paths);
        }
    }

    /// Fork: the strings of a Java `String[]` a native method was handed;
    /// none when it cannot be read.
    ///
    /// # Safety
    ///
    /// `array` must be a `String[]` argument of the Java call this runs in.
    unsafe fn strings_of(array: sys::jobjectArray) -> Vec<String> {
        let read = JavaVM::singleton().and_then(|vm| {
            vm.attach_current_thread(|env| {
                // SAFETY: the caller's promise.
                let array = unsafe { JObjectArray::<JString>::from_raw(env, array) };
                let mut out = Vec::new();
                for index in 0..array.len(env)? {
                    let item = array.get_element(env, index)?;
                    out.push(item.mutf8_chars(env)?.to_string());
                }
                Ok::<_, jni::errors::Error>(out)
            })
        });
        read.unwrap_or_else(|error| {
            log::warn!("import: reading what the copy said failed: {error}");
            Vec::new()
        })
    }

    /// Fork: `ConcatFiles.folderPicked`, on the activity's thread: {path},
    /// {} for nothing, or {"", why}.
    pub(super) unsafe extern "system" fn folder_picked(
        _env: *mut sys::JNIEnv,
        _class: sys::jclass,
        answer: sys::jobjectArray,
    ) {
        // SAFETY: `answer` is the String[] Java handed this frame.
        let answer = unsafe { strings_of(answer) };
        let result = match answer.as_slice() {
            [path] if !path.is_empty() => Ok(Some(PathBuf::from(path))),
            [_, why] => Err(why.clone()),
            _ => Ok(None),
        };
        match FOLDER_PENDING.lock().ok().and_then(|mut slot| slot.take()) {
            Some(pending) => pending(result),
            None => log::warn!("project: a folder was picked with nobody waiting for it"),
        }
    }

    /// Fork: `ConcatFiles.importProgress`, on the thread the Java copies on.
    pub(super) unsafe extern "system" fn import_progress(
        _env: *mut sys::JNIEnv,
        _class: sys::jclass,
        name: sys::jobjectArray,
        index: sys::jint,
        count: sys::jint,
        copied: sys::jlong,
        total: sys::jlong,
    ) {
        // SAFETY: `name` is the String[] Java handed this frame.
        let name = unsafe { strings_of(name) }.into_iter().next().unwrap_or_default();
        concat::import_copy_progress(concat::ImportCopy {
            name,
            index: usize::try_from(index).unwrap_or(0),
            count: usize::try_from(count).unwrap_or(1).max(1),
            copied: u64::try_from(copied).unwrap_or(0),
            total: u64::try_from(total).ok().filter(|total| *total > 0),
        });
    }

    /// Fork: `ConcatFiles.importProblem`: a picked file that was not
    /// copied, and why.
    pub(super) unsafe extern "system" fn import_problem(
        _env: *mut sys::JNIEnv,
        _class: sys::jclass,
        what: sys::jobjectArray,
        size: sys::jlong,
        free: sys::jlong,
    ) {
        // SAFETY: `what` is the String[] Java handed this frame.
        let what = unsafe { strings_of(what) };
        let field = |index: usize| what.get(index).cloned().unwrap_or_default();
        let problem = match field(0).as_str() {
            "space" => concat::ImportProblem::NoSpace {
                name: field(1),
                size: u64::try_from(size).unwrap_or(0),
                free: u64::try_from(free).unwrap_or(0),
            },
            "cancelled" => concat::ImportProblem::Cancelled,
            _ => concat::ImportProblem::Failed {
                name: field(1),
                reason: field(2),
            },
        };
        concat::import_copy_problem(problem);
    }
}

/// Where a finished export goes.
///
/// The export writes into the app's own folder, which on a phone nothing
/// but the app can open: not the gallery, not a file manager (#280,
/// #147). So once a file is written the Java moves it into the phone's
/// Movies, under Concat, through the media store, and the sheet names
/// that folder from the start. A phone older than Android 10 has no
/// media store an app can write through without a permission; there the
/// file stays where it was written, and the sheet says so.
#[cfg(target_os = "android")]
mod publisher {
    use std::path::Path;

    use jni::objects::{JObjectArray, JString};
    use jni::refs::Reference;
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// Hands the window's crate the way to the media store, where this
    /// phone has one to write through.
    pub fn install(app: &AndroidApp) {
        let folder = match published_folder(app) {
            Ok(Some(folder)) => folder,
            Ok(None) => {
                log::info!(
                    "export: finished exports stay in the app's folder · this phone's media store takes no files"
                );
                return;
            }
            Err(error) => {
                log::warn!(
                    "export: asking the phone where exports go failed: {error}; they stay in the app's folder"
                );
                return;
            }
        };
        let app = app.clone();
        let documents = app.clone();
        concat::install_export_publisher(concat::ExportPublisher {
            folder,
            publish: Box::new(move |path| publish(&app, path)),
            publish_document: Box::new(move |path, mime| publish_document(&documents, path, mime)),
        });
    }

    /// `ConcatFiles.publishedFolder`: "Movies/Concat", or nothing.
    fn published_folder(app: &AndroidApp) -> jni::errors::Result<Option<String>> {
        super::java::with_class(app, |env, _activity, class| {
            let answer = env
                .call_static_method(
                    class,
                    jni_str!("publishedFolder"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            if answer.is_null() {
                return Ok(None);
            }
            let answer = env.cast_local::<JString>(answer)?;
            Ok(Some(answer.mutf8_chars(env)?.to_string()))
        })
    }

    /// `ConcatFiles.publishVideo`: the file's path under the phone's
    /// storage once it is in Movies, or why it could not be moved.
    fn publish(app: &AndroidApp, path: &Path) -> Result<String, String> {
        answer(super::java::with_class(app, |env, activity, class| {
            let path = env.new_string(path.to_string_lossy())?;
            let answer = env
                .call_static_method(
                    class,
                    jni_str!("publishVideo"),
                    jni_sig!("(Landroid/app/Activity;Ljava/lang/String;)[Ljava/lang/String;"),
                    &[activity.into(), (&path).into()],
                )?
                .l()?;
            strings(env, answer)
        }))
    }

    /// `ConcatFiles.publishDocument`: the file's path under the phone's
    /// storage once it is in Download/Concat, or why it could not be moved.
    fn publish_document(app: &AndroidApp, path: &Path, mime: &str) -> Result<String, String> {
        answer(super::java::with_class(app, |env, activity, class| {
            let path = env.new_string(path.to_string_lossy())?;
            let mime = env.new_string(mime)?;
            let answer = env
                .call_static_method(
                    class,
                    jni_str!("publishDocument"),
                    jni_sig!("(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;)[Ljava/lang/String;"),
                    &[activity.into(), (&path).into(), (&mime).into()],
                )?
                .l()?;
            strings(env, answer)
        }))
    }

    /// A Java `String[]` as Rust's, a null element as None.
    pub(super) fn strings<'local>(
        env: &mut jni::Env<'local>,
        answer: jni::objects::JObject<'local>,
    ) -> jni::errors::Result<Vec<Option<String>>> {
        let answer = env.cast_local::<JObjectArray<JString>>(answer)?;
        let mut out = Vec::new();
        for index in 0..answer.len(env)? {
            let item = answer.get_element(env, index)?;
            out.push(if item.is_null() {
                None
            } else {
                Some(item.mutf8_chars(env)?.to_string())
            });
        }
        Ok(out)
    }

    /// What `publishVideo` and `publishDocument` answer: where the file is
    /// now, or why it is still where it was.
    fn answer(answer: jni::errors::Result<Vec<Option<String>>>) -> Result<String, String> {
        let answer =
            answer.map_err(|error| format!("could not reach the phone's media store: {error}"))?;
        match answer.as_slice() {
            [Some(path), _] => Ok(path.clone()),
            [None, Some(why)] => Err(why.clone()),
            _ => Err("the phone said nothing".to_owned()),
        }
    }
}

/// The microphone, for a voiceover.
///
/// Android grants it while the app runs, not at install, and an input
/// stream opened without it hears only silence; so the voiceover panel
/// asks here before it opens one, and the Java puts the system's question
/// up when the answer is not yet yes.
#[cfg(target_os = "android")]
mod microphone {
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// Hands the window's crate the way to ask.
    pub fn install(app: &AndroidApp) {
        let app = app.clone();
        concat::install_microphone_access(Box::new(move || {
            super::java::with_class(&app, |env, activity, class| {
                env.call_static_method(
                    class,
                    jni_str!("askMicrophone"),
                    jni_sig!("(Landroid/app/Activity;)Z"),
                    &[activity.into()],
                )?
                .z()
            })
            .unwrap_or_else(|error| {
                // Not knowing is not a no: the device is opened, and its
                // own error, or its silence, says the rest.
                log::warn!(
                    "voice: asking for the microphone failed: {error}; it is opened all the same"
                );
                true
            })
        }));
    }
}

/// Fork: picked media read where they lie - the phone's storage, an SD
/// card - instead of a copy in the app: whether the phone lets the app do
/// that, and the asking; see ConcatFiles.filesAccess.
#[cfg(target_os = "android")]
mod files_access {
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// Hands the window's crate the way to ask.
    pub fn install(app: &AndroidApp) {
        let app = app.clone();
        concat::install_files_access(Box::new(move |ask| {
            super::java::with_class(&app, |env, activity, class| {
                env.call_static_method(
                    class,
                    jni_str!("filesAccess"),
                    jni_sig!("(Landroid/app/Activity;Z)I"),
                    &[activity.into(), ask.into()],
                )?
                .i()
            })
            .unwrap_or_else(|error| {
                // Not knowing is a no: picks are copied, as they always were.
                log::warn!(
                    "import: asking whether files can be read where they lie failed: {error}; they are copied"
                );
                0
            })
        }));
    }
}

/// The browser, for the launch screen's guides and links: an ACTION_VIEW
/// intent through the activity, which the Java sends.
#[cfg(target_os = "android")]
mod links {
    use jni::objects::JString;
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// Hands the window's crate the way to open a web address.
    pub fn install(app: &AndroidApp) {
        let app = app.clone();
        concat::install_link_opener(Box::new(move |url| {
            let answer = super::java::with_class(&app, |env, activity, class| {
                let url = env.new_string(url)?;
                let answer = env
                    .call_static_method(
                        class,
                        jni_str!("openLink"),
                        jni_sig!("(Landroid/app/Activity;Ljava/lang/String;)Ljava/lang/String;"),
                        &[activity.into(), (&url).into()],
                    )?
                    .l()?;
                if answer.is_null() {
                    return Ok(None);
                }
                let answer = env.cast_local::<JString>(answer)?;
                Ok(Some(answer.mutf8_chars(env)?.to_string()))
            });
            match answer {
                Ok(None) => Ok(()),
                Ok(Some(why)) => Err(why),
                Err(error) => Err(format!("could not reach the browser: {error}")),
            }
        }));
    }
}

/// The log, for a report from a phone (#560).
///
/// A phone has no folder to show the log in, so Settings › About hands it
/// to the share sheet, and so does the notice a launch puts up when the
/// last run died. Which run died is the system's to say: a run on a phone
/// is almost never closed in order - it is swiped away, or put down in the
/// background - so a log without its last line proves nothing here. From
/// Android 11 the system keeps a record of how each process ended, and the
/// app may mark its own with a few bytes; each run marks itself with its
/// log's name, so the next one reads which file the record is about.
#[cfg(target_os = "android")]
mod log_file {
    use jni::objects::JString;
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    /// Hands the window's crate the share sheet, and tells it how the last
    /// run ended. After the backend, which seeds the JavaVM.
    pub fn install(app: &AndroidApp) {
        let sharer = app.clone();
        concat::install_log_sharer(Box::new(move |paths: &[std::path::PathBuf]| {
            let joined = paths
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\n");
            let answer = super::java::with_class(&sharer, |env, activity, class| {
                let joined = env.new_string(&joined)?;
                let answer = env
                    .call_static_method(
                        class,
                        jni_str!("shareLogs"),
                        jni_sig!("(Landroid/app/Activity;Ljava/lang/String;)Ljava/lang/String;"),
                        &[activity.into(), (&joined).into()],
                    )?
                    .l()?;
                if answer.is_null() {
                    return Ok(None);
                }
                let answer = env.cast_local::<JString>(answer)?;
                Ok(Some(answer.mutf8_chars(env)?.to_string()))
            });
            match answer {
                Ok(None) => Ok(()),
                Ok(Some(why)) => Err(why),
                Err(error) => Err(format!("could not reach the share sheet: {error}")),
            }
        }));
        read_last_exit(app);
    }

    /// Asks the system how the last run ended, says so in the log, and
    /// marks this run with its log's name.
    fn read_last_exit(app: &AndroidApp) {
        let this = concat::log_file_name().unwrap_or_default();
        let answer = super::java::with_class(app, |env, activity, class| {
            let this = env.new_string(&this)?;
            let answer = env
                .call_static_method(
                    class,
                    jni_str!("lastExit"),
                    jni_sig!("(Landroid/app/Activity;Ljava/lang/String;)[Ljava/lang/String;"),
                    &[activity.into(), (&this).into()],
                )?
                .l()?;
            if answer.is_null() {
                return Ok(None);
            }
            super::publisher::strings(env, answer).map(Some)
        });
        match answer.as_ref().map(|answer| answer.as_deref()) {
            Ok(Some([Some(log), Some(verdict), Some(detail), threads])) => {
                let died = verdict == "died";
                let run = if log.is_empty() {
                    "unmarked"
                } else {
                    log.as_str()
                };
                if died {
                    log::warn!("last run: {run} died · the system says: {detail}");
                } else {
                    log::info!("last run: {run} ended · the system says: {detail}");
                }
                // A hang's threads as the system found them, one entry
                // under the line above: what the window was doing instead
                // of answering, frames in Concat's library as offsets in
                // it (docs/LOGGING.md, "Reading a phone's hang").
                if let Some(threads) = threads.as_deref().filter(|text| !text.trim().is_empty()) {
                    log::warn!(
                        "last run: the threads of {run} when the system gave up on it\n{threads}"
                    );
                }
                concat::note_last_exit(log.clone(), died);
            }
            Ok(None) => log::debug!("last run: the system keeps no record of it"),
            Ok(Some(other)) => log::warn!("last run: the system's record made no sense: {other:?}"),
            Err(error) => log::warn!("last run: asking the system how it ended failed: {error}"),
        }
    }
}

/// The screen kept on while a job runs - an export, a transcription, a
/// download - so a phone left on the table does not time out half a
/// minute in and stop the export with it (#695). The window's flag, which
/// the activity applies on its own thread.
///
/// Through the activity's Java, never `AndroidApp::set_window_flags`: that
/// takes the app's lock for writing, and the window's thread is inside
/// `poll_events`, which holds the same lock for reading for as long as an
/// event is handled - and an event is where a job starts. Called from
/// Export, the window's thread waited on itself for good, and Android
/// closed Concat as not responding (#747, #786, #760, #744; 0.2.10 and
/// 0.2.11, since #695). Off that thread it is no better: a writer waiting
/// there blocks the next read the window's thread takes inside its event.
/// The Java posts to the activity's thread and returns.
#[cfg(target_os = "android")]
mod awake {
    use jni::{jni_sig, jni_str};
    use slint::android::AndroidApp;

    pub fn install(app: &AndroidApp) {
        let app = app.clone();
        concat::install_awake(Box::new(move |on| {
            log::debug!("display: the screen kept on while a job runs: {on}");
            let asked = super::java::with_class(&app, |env, activity, class| {
                env.call_static_method(
                    class,
                    jni_str!("keepScreenOn"),
                    jni_sig!("(Landroid/app/Activity;Z)V"),
                    &[activity.into(), on.into()],
                )?;
                Ok(())
            });
            if let Err(error) = asked {
                log::warn!(
                    "display: could not keep the screen on while a job runs: {error}; it may go dark part way"
                );
            }
        }));
    }
}

/// Called by the activity's native glue; the name is the contract.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    // Directories first: the log file is written into one of them.
    activity::name_directories(&app);
    activity::open_log();
    log::info!("Concat {} starting", env!("CARGO_PKG_VERSION"));
    if let Err(error) = slint::android::init(app.clone()) {
        // "could not start" right after the topic: the smoke script looks for it.
        log::error!("start: could not start the Android backend: {error}; the app closes");
        return;
    }
    // After the backend, which seeds the JavaVM the Java is reached by.
    picker::install(&app);
    publisher::install(&app);
    microphone::install(&app);
    files_access::install(&app);
    links::install(&app);
    awake::install(&app);
    log_file::install(&app);
    if let Err(error) = concat::run() {
        log::error!("close: the window ended in an error: {error}; the app closes");
    }
}
