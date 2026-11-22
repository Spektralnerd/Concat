//! What FFmpeg's static archives need from a phone.
//!
//! On the desktop FFmpeg arrives as shared libraries that carry their own
//! dependencies. A phone links the archives that
//! `scripts/ffmpeg-mobile.sh` builds, and an archive brings nothing with
//! it: the platform libraries its code calls into - zlib, the hardware
//! codec bridges, and on Android the JNI shim - have to be named here.
//! The list is the `Libs:` line of the pkg-config files that build writes.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match os.as_str() {
        // MediaCodec through the NDK, the camera device, and the JNI it all
        // rides on. libatomic backs the lock-free counters FFmpeg uses on
        // AArch64.
        "android" => {
            for lib in ["z", "m", "atomic", "android", "mediandk", "camera2ndk"] {
                println!("cargo:rustc-link-lib={lib}");
            }
            // Fork: libopus, the Opus encoder ffmpeg-mobile.sh builds into
            // FFmpeg's prefix beside the archives (FFmpeg 8.1's own writes
            // broken sound), when the FFmpeg linked is one that has it.
            // Watched by path, so a rebuilt prefix that gains it is seen.
            println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
            if let Some(dir) = std::env::var_os("FFMPEG_DIR") {
                let lib = std::path::Path::new(&dir).join("lib");
                let opus = lib.join("libopus.a");
                println!("cargo:rerun-if-changed={}", opus.display());
                if opus.is_file() {
                    println!("cargo:rustc-link-search=native={}", lib.display());
                    println!("cargo:rustc-link-lib=static=opus");
                }
            }
        }
        // VideoToolbox and AudioToolbox, the hardware codecs, the
        // frameworks they hand buffers through, and Security for the TLS
        // behind https.
        "ios" => {
            for lib in ["z", "m"] {
                println!("cargo:rustc-link-lib={lib}");
            }
            for framework in [
                "VideoToolbox",
                "AudioToolbox",
                "CoreMedia",
                "CoreVideo",
                "CoreFoundation",
                "Foundation",
                "Security",
            ] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        }
        _ => {}
    }
}
