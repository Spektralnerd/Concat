#!/usr/bin/env bash
# Builds FFmpeg's libraries for a phone, from source, as static archives
# the engine links.
#
#   scripts/ffmpeg-mobile.sh aarch64-linux-android [out-dir]
#   scripts/ffmpeg-mobile.sh x86_64-linux-android [out-dir]   (the emulator's, never shipped)
#   scripts/ffmpeg-mobile.sh aarch64-apple-ios     [out-dir]
#   scripts/ffmpeg-mobile.sh aarch64-apple-ios-sim [out-dir]
#
# The result is a prefix - include/ and lib/ - that concat-media's
# bindings take through FFMPEG_DIR, exactly as they take a Homebrew or
# BtbN build on the desktop. It lands in vendor/ffmpeg/<target> under the
# engine by default, outside target/, so a `cargo clean` does not cost
# another FFmpeg build and CI's cache can keep it.
#
# Android needs the NDK (ANDROID_NDK_HOME, or the newest one under the SDK
# in ANDROID_HOME / ~/Library/Android/sdk); iOS needs Xcode. Both builds
# are LGPL FFmpeg with the platform's hardware codecs turned on -
# MediaCodec through the JNI on Android, VideoToolbox on iOS - and nothing
# else linked in: a phone encodes with its silicon, and the GPL encoders
# the desktop bundles have no place there.
#
# Fork: one exception, on Android - libopus (BSD), built first into the
# same prefix as a static archive. FFmpeg 8.1's own Opus encoder writes a
# few good seconds and then noise and silence (upstream mended it in
# FFmpeg 9: "fix CELT psy analysis and packet sizing"), and Opus is a
# codec an export on the phone offers. FFmpeg finds libopus by the
# pkg-config file it installs, and concat-media's build.rs links it.
set -euo pipefail

target=${1:?target triple: aarch64-linux-android, x86_64-linux-android, aarch64-apple-ios or aarch64-apple-ios-sim}
workspace=$(cd "$(dirname "$0")/.." && pwd)
out=${2:-$workspace/vendor/ffmpeg/$target}
version=${FFMPEG_VERSION:-8.1}
# Oldest OS each build runs on. Android 8.0 is where AAudio, the audio
# path the engine plays through, appears; iOS 15 is where Slint draws.
android_api=${ANDROID_API:-26}
ios_min=${IPHONEOS_DEPLOYMENT_TARGET:-15.0}
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)

work=${FFMPEG_WORK_DIR:-$workspace/vendor/ffmpeg/src}
src=$work/ffmpeg-$version
mkdir -p "$work"
if [ ! -f "$src/configure" ]; then
  echo "Fetching FFmpeg $version"
  curl -fsSL --retry 5 --retry-all-errors -o "$work/ffmpeg-$version.tar.xz" \
    "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz"
  tar -xf "$work/ffmpeg-$version.tar.xz" -C "$work"
fi

# Fork: libopus into the prefix, as a static archive with its pkg-config
# file, built by CMake with the toolchain arguments given. The release is
# pinned by digest in scripts/ci/deps.lock (the row named opus-source,
# whose URL names this version too). Floating point, as FFmpeg's wrapper
# hands it floats; no programs, no tests, none of 1.5's neural extras.
opus_version=1.5.2
build_opus() {
  local opus_src=$work/opus-$opus_version
  local opus_build=$work/opus-build-$target
  if [ ! -f "$opus_src/CMakeLists.txt" ]; then
    echo "Fetching libopus $opus_version"
    "$workspace/../scripts/ci/fetch.sh" opus-source "$work/opus-$opus_version.tar.gz"
    tar -xzf "$work/opus-$opus_version.tar.gz" -C "$work"
  fi
  echo "Building libopus $opus_version for $target"
  rm -rf "$opus_build"
  cmake -S "$opus_src" -B "$opus_build" "$@" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$out" \
    -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_INSTALL_INCLUDEDIR=include \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DBUILD_SHARED_LIBS=OFF -DOPUS_BUILD_SHARED_LIBRARY=OFF \
    -DOPUS_BUILD_TESTING=OFF -DOPUS_BUILD_PROGRAMS=OFF
  cmake --build "$opus_build" --parallel "$jobs"
  cmake --install "$opus_build"
}

# One build tree per target, so two phones can be built from one source.
build=$work/build-$target
rm -rf "$build"
mkdir -p "$build"

# Flags shared by every phone build. No programs, no docs, no shared
# libraries: the engine links the archives into one binary. Position
# independent because Android loads the app as a shared object.
common=(
  --prefix="$out"
  --enable-cross-compile
  --enable-static --disable-shared --enable-pic
  --disable-programs --disable-doc
  --disable-debug
  # Nothing external. zlib is part of both platforms' SDKs; bzip2 and
  # lzma are on one and not the other, so neither is taken.
  --enable-zlib --disable-bzlib --disable-lzma
  --disable-iconv --disable-sdl2 --disable-xlib --disable-libxcb
  --disable-vulkan --disable-opencl --disable-vaapi --disable-vdpau
)

case "$target" in
  aarch64-linux-android|x86_64-linux-android)
    ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
    if [ -z "$ndk" ]; then
      sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}
      ndk=$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1 || true)
    fi
    [ -d "$ndk" ] || { echo "no Android NDK: set ANDROID_NDK_HOME" >&2; exit 1; }
    host_tag=$(ls "$ndk/toolchains/llvm/prebuilt" | head -1)
    bin=$ndk/toolchains/llvm/prebuilt/$host_tag/bin
    echo "Building FFmpeg $version for $target with NDK at $ndk (API $android_api)"
    # The x86_64 build is the one CI's emulators can run, never shipped:
    # FFmpeg's hand-written assembly is left out there, so the build needs
    # no assembler, and its speed is nobody's concern.
    if [ "$target" = aarch64-linux-android ]; then
      arch=(--arch=aarch64 --cpu=armv8-a)
      abi=arm64-v8a
    else
      arch=(--arch=x86_64 --disable-x86asm)
      abi=x86_64
    fi
    # Fork: libopus first, by the NDK's own CMake toolchain, for the same
    # ABI and API level as FFmpeg.
    build_opus \
      -DCMAKE_TOOLCHAIN_FILE="$ndk/build/cmake/android.toolchain.cmake" \
      -DANDROID_ABI="$abi" -DANDROID_PLATFORM="android-$android_api"
    # Fork: FFmpeg's encoder over it - not its decoder, as FFmpeg's own
    # decodes Opus well. pkg-config is pointed at the prefix alone, so
    # nothing of the build machine's own is found and linked.
    (cd "$build" && PKG_CONFIG_LIBDIR="$out/lib/pkgconfig" "$src/configure" "${common[@]}" \
      --target-os=android "${arch[@]}" \
      --sysroot="$ndk/toolchains/llvm/prebuilt/$host_tag/sysroot" \
      --cc="$bin/$target$android_api-clang" \
      --cxx="$bin/$target$android_api-clang++" \
      --ar="$bin/llvm-ar" --nm="$bin/llvm-nm" --ranlib="$bin/llvm-ranlib" --strip="$bin/llvm-strip" \
      --enable-jni --enable-mediacodec \
      --pkg-config=pkg-config --pkg-config-flags=--static \
      --enable-libopus --disable-decoder=libopus \
      --extra-cflags="-fno-omit-frame-pointer" \
      --extra-ldflags="-Wl,-z,max-page-size=16384")
    ;;
  aarch64-apple-ios|aarch64-apple-ios-sim)
    if [ "$target" = aarch64-apple-ios ]; then
      sdk=iphoneos; min_flag="-miphoneos-version-min=$ios_min"
    else
      sdk=iphonesimulator; min_flag="-mios-simulator-version-min=$ios_min"
    fi
    sysroot=$(xcrun --sdk $sdk --show-sdk-path)
    echo "Building FFmpeg $version for $target against $sysroot"
    (cd "$build" && "$src/configure" "${common[@]}" \
      --target-os=darwin --arch=arm64 \
      --sysroot="$sysroot" \
      --cc="$(xcrun --sdk $sdk -f clang)" \
      --cxx="$(xcrun --sdk $sdk -f clang++)" \
      --ar="$(xcrun --sdk $sdk -f ar)" --nm="$(xcrun --sdk $sdk -f nm)" \
      --ranlib="$(xcrun --sdk $sdk -f ranlib)" --strip="$(xcrun --sdk $sdk -f strip)" \
      --enable-videotoolbox --enable-audiotoolbox \
      --disable-avfoundation --disable-coreimage \
      --extra-cflags="-arch arm64 $min_flag" \
      --extra-ldflags="-arch arm64 $min_flag")
    ;;
  *)
    echo "unknown target $target" >&2; exit 1 ;;
esac

make -C "$build" -j"$jobs" install
echo "FFmpeg $version for $target is in $out"
echo "  export FFMPEG_DIR=$out"
