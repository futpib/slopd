#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -f "${ANDROID_NDK_HOME:-}/source.properties" ]]; then
    export ANDROID_NDK_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}/ndk/28.2.13676358"
fi
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --manifest-path native/Cargo.toml --release --locked
