#!/usr/bin/env bash
# Build the Android native libraries (arm64-v8a, x86_64) and assemble the Android APK.
#
# Usage: packaging/android/package.sh [--skip-gradle]
#
# Needs: cargo-ndk (`cargo install cargo-ndk --locked`), Android NDK (`ANDROID_NDK_HOME`),
# and the `aarch64-linux-android` + `x86_64-linux-android` Rust targets.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/android"
JNI_LIBS="$HERE/app/src/main/jniLibs"

command -v cargo-ndk >/dev/null || { echo "error: cargo-ndk not found (cargo install cargo-ndk --locked)" >&2; exit 1; }

mkdir -p "$JNI_LIBS"
(
  cd "$ROOT"
  cargo ndk \
    -t arm64-v8a \
    -t x86_64 \
    -o "$JNI_LIBS" \
    build --release --locked -p filmcraft-android
)

if [ "${1:-}" != "--skip-gradle" ]; then
  if command -v gradle >/dev/null; then
    (cd "$HERE" && gradle assembleRelease)
    APK="$HERE/app/build/outputs/apk/release/app-release.apk"
    if [ -f "$APK" ]; then
      cp "$APK" "$DIST/filmcraft-$VERSION-android.apk"
      echo "packaged: $DIST/filmcraft-$VERSION-android.apk"
    fi
  else
    echo "note: gradle not on PATH; native libraries staged in $JNI_LIBS"
  fi
fi
